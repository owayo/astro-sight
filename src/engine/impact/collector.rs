//! Pass 2 (streaming fold) で使う per-worker 状態と visitor 実装。
//!
//! - `WorkerState`: rayon fold の local state。chunk 内で再利用され、chunk 終了時に
//!   merge → drop される。
//! - `RefEventMini`: 1 件の reference event を最小サイズで保持する内部 struct。
//!   `on_ref` 時点で import 判定と caller_name intern まで済ませておき、context の
//!   文字列コピーを廃して per-file バッファの heap を削減する。
//! - `ImpactCollector`: `refs::RefVisitor` を実装し、per-file visit の callback を
//!   直接受け取って `finish_file` で Stage 1-6 (Stage 4b 除く) を適用する。
use lru::LruCache;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::engine::lexical_binding::LexicalBinding;
use crate::engine::refs;
use crate::language::LangId;

use super::import_facts::{
    RefFileFacts, context_line_has_macro_invocation, is_rust_local_shadow_context,
    is_ts_local_shadow_context, lang_is_ts_family, ref_file_directly_imports_source,
    rust_ref_file_has_symbol_evidence, should_route_rust_ref_low,
    should_route_ts_importless_ref_low,
};
use super::signature::{extract_function_from_context, same_top_level_arity};
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
    /// import-only など blocking にはしない情報提供 caller の振り分け先。
    pub(super) local_informational_maps: Vec<TypedCallerMap>,
    pub(super) local_def_paths: Vec<Vec<u32>>,
    pub(super) target_cache: LruCache<String, Option<ParsedFile>>,
    /// TS/TSX/JS の名前衝突 false positive 抑制用 (Issue
    /// 2026-06-05-multi-attachment-conversations-fp 対応)。ref file ごとの import specifier
    /// basename 集合をキャッシュする。fail-closed: 構築失敗時は `None` を格納し、その ref は
    /// 「直接 import あり」相当として high impact 維持。
    pub(super) import_facts_cache: LruCache<String, Option<RefFileFacts>>,
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
    pub(super) rust_alias_import_used: bool,
    /// 0 = ExactOwner, 1 = InferredOwner, 2 = BareNameOnly。
    /// Phase 4 (impact ルーティング) で `BareNameOnly` + generic name のシンボル参照を
    /// `low_confidence_callers` へ振り分けるために使う。
    pub(super) confidence: u8,
    /// TS/TSX/JS / Rust の context 行で identifier がローカル binding で shadow されているか。
    /// `finish_file` の high vs low routing に使う (Issue 2026-06-05-multi-attachment-conversations-fp /
    /// Issue 2026-06-13-ai-status-json-symbol-fp)。
    pub(super) local_shadow_hint: bool,
    /// Barrel origin evidence only applies to a value/type use of the imported binding.
    pub(super) ts_import_binding_use: bool,
    /// Rust: context 行に macro 呼び出し (`ident!(` 等) があるか。証拠なしの bare identifier でも
    /// macro 引数なら macro 展開で path が補われ得るため high 維持する (fail-closed)。
    pub(super) ref_in_macro: bool,
    /// Rust: 参照 identifier が `json!()` の callee 名そのものか。通常の関数/値シンボル変更では
    /// macro callee は別名前空間なので low に落とす一方、macro 引数は `ref_in_macro` で high 維持する。
    pub(super) rust_macro_callee: bool,
    /// `receiver.name` の field 位置。自由関数はこの構文で参照できない。
    pub(super) rust_receiver_member: bool,
    /// Receiver member, associated path, or macro token that could name a method.
    pub(super) rust_method_syntax: bool,
    /// Written receiver type, or enclosing impl type for `self`; u32::MAX if unknown.
    pub(super) rust_explicit_method_owner_id: u32,
    /// AST and project evidence identify this Rust reference as a standard crate name.
    pub(super) rust_std_origin: bool,
    /// Rust: 参照が callee でない関数値渡し (`register((a, my_system, b))` のタプル要素等)。
    /// シグネチャのみ変更 (arity 不変) の場合、トレイト境界に吸収されコンパイルが通る
    /// ことが多いため informational へ格下げする (Issue 2026-07-12 Bevy systemparam FP)。
    pub(super) fn_value_ref: bool,
    /// dot chain の直接読み出しパス。対象外は u32::MAX、文字列は既存 pool で共有する。
    pub(super) member_path_id: u32,
    pub(super) jsx_shape_id: u32,
    /// 別名のない named import/export 以外は、全体利用の範囲を証明しない。
    pub(super) unproven_module_ref: bool,
    /// Rust 値束縛の証拠は変更元ごとの kind を確認して適用する。
    pub(super) rust_value_binding: bool,
    /// Python の相対 import で直接定義へ解決した所有元。未証明は u32::MAX。
    pub(super) python_import_owner_id: u32,
    /// Rust type path structurally resolved to a local declaration file.
    pub(super) rust_owner_path_id: u32,
    pub(super) rust_owner_is_free_function: bool,
    pub(super) rust_associated_type_path: bool,
    /// A same-name direct declaration in the reference file owns this use.
    pub(super) rust_local_owner: bool,
    pub(super) rust_direct_import_site: bool,
    pub(super) rust_import_binding_safe: bool,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CallerRoute {
    Normal,
    Low,
    Informational,
}

/// 参照1件を出力バケットへ振り分ける際に、ファイル全体で共有する情報。
/// 集約mapや文字列poolは持たず、判定に必要なimport事実キャッシュだけを更新する。
struct RouteDecisionContext<'a> {
    filter_disabled: bool,
    ref_lang: Option<LangId>,
    dir: &'a str,
    path_str: &'a str,
    import_facts_cache: &'a mut LruCache<String, Option<RefFileFacts>>,
    reexport_moves: &'a super::reexport_move::ReexportMoveIndex,
    ts_barrels: &'a super::ts_barrels::TsBarrelIndex,
}

impl RouteDecisionContext<'_> {
    fn decide(
        &mut self,
        event: &RefEventMini,
        file_context: &FileContext,
        symbol_name: &str,
        has_parent_type: bool,
        base_route_low: bool,
        member_unchanged: bool,
    ) -> CallerRoute {
        // callee でない関数値渡し参照 (`register((a, my_system, b))` のタプル要素等) は、
        // シグネチャのみ変更 (arity 不変) ならトレイト境界に吸収されコンパイルが通る
        // ことが多いため informational へ。arity 変更は高階 API でも壊れ得るため、
        // また removed (存在破壊) は sig_changes に載らないため、どちらも blocking 維持。
        let kind_changed = file_context.rust_kind_changes.contains(symbol_name);
        let fn_value_informational = !kind_changed
            && event.fn_value_ref
            && file_context.sig_changes.iter().any(|change| {
                ci_key(file_context.lang_id, &change.name) == symbol_name
                    && same_top_level_arity(&change.old_signature, &change.new_signature)
            });
        // 定義が影響元ファイルへ移動し、この参照ファイルが旧公開パスを `pub use` で
        // 維持している場合、参照は同一 diff 内で新定義へ解決済みなので修正箇所は無い。
        // 成立条件 (自前定義の削除 + 再輸出の追加 + 移動先の定義追加 + パス解決) は
        // `ReexportMoveIndex` 側で連言として検証する。
        let reexport_move_informational = self.ref_lang == Some(LangId::Rust)
            && file_context.lang_id == LangId::Rust
            && self
                .reexport_moves
                .covers(self.path_str, symbol_name, &file_context.new_path);

        // old/new のフルソース AST で透過的な root type alias 導入だけと証明できた
        // シグネチャ変更は、caller を消さず informational に残す。
        let transparent_alias_informational =
            file_context.compatible_signature_keys.contains(symbol_name);

        let route_informational = transparent_alias_informational
            || fn_value_informational
            || reexport_move_informational
            || (event.is_import
                && !event.rust_alias_import_used
                && !(event.unproven_module_ref
                    && file_context.object_member_changes.contains_key(symbol_name))
                && file_context.affected.iter().any(|symbol| {
                    symbol.change_type == "modified"
                        && ci_key(file_context.lang_id, &symbol.name) == symbol_name
                }));

        // import-only 参照は modified シンボルだけ情報提供に分離する。
        // removed/rename 相当の import は存在破壊になり得るため通常側に残す。
        let route_low = if event.is_import {
            false
        } else {
            // TS/TSX/JS 名前衝突の誤検出抑制:
            // 参照元と変更元がTS系で、変更元moduleの直接importがなければlowへ振り分ける。
            let ts_route_low = if !self.filter_disabled
                && self.ref_lang.is_some_and(lang_is_ts_family)
                && lang_is_ts_family(file_context.lang_id)
            {
                let has_direct_import = ref_file_directly_imports_source(
                    self.dir,
                    self.path_str,
                    &file_context.new_path,
                    self.import_facts_cache,
                );
                should_route_ts_importless_ref_low(
                    true,
                    true,
                    has_direct_import
                        || (event.ts_import_binding_use
                            && !event.local_shadow_hint
                            && self.ts_barrels.proves_origin(
                                self.path_str,
                                symbol_name,
                                &file_context.new_path,
                            )),
                    event.local_shadow_hint,
                )
            } else {
                false
            };

            // Rust 名前衝突の誤検出抑制:
            // 自由関数は別moduleからbareで呼べないため、import/qualified path/globの証拠が
            // なければlowへ振り分ける。メソッドはbare呼び出しが正当なので対象外。
            let rust_route_low = if !self.filter_disabled
                && !has_parent_type
                && self.ref_lang == Some(LangId::Rust)
                && file_context.lang_id == LangId::Rust
            {
                if event.rust_macro_callee {
                    true
                } else {
                    match rust_ref_file_has_symbol_evidence(
                        self.dir,
                        self.path_str,
                        symbol_name,
                        self.import_facts_cache,
                    ) {
                        Some(has_evidence) => should_route_rust_ref_low(
                            has_evidence,
                            // 関数向けの確定束縛は除外済み。行全体の推測で初期化子等を降格しない。
                            event.local_shadow_hint
                                && !rust_function_only(file_context, symbol_name)
                                && !kind_changed,
                            event.ref_in_macro,
                        ),
                        None => false,
                    }
                }
            } else {
                false
            };
            base_route_low || ts_route_low || rust_route_low
        };

        if route_informational {
            CallerRoute::Informational
        } else if route_low {
            CallerRoute::Low
        } else if member_unchanged {
            CallerRoute::Informational
        } else {
            CallerRoute::Normal
        }
    }
}

/// `RefVisitor` の実装: per-file の ref 走査中は最小限の buffering だけ行い、
/// ファイル走査完了後に `finish_file` で Stage 1-6 (Stage 4b 除く) の filter を適用して
/// `local_maps` / `local_def_paths` へ流す。`SymbolReference` の Vec は生成しない。
pub(super) struct ImpactCollector<'a> {
    pub(super) jsx_shapes: Vec<super::jsx_props::JsxShape>,
    pub(super) jsx_parse_clean: std::cell::OnceCell<bool>,
    pub(super) python_imports:
        std::cell::OnceCell<Option<super::python_imports::PythonImportIndex>>,
    pub(super) rust_std_origin:
        std::cell::OnceCell<Option<super::rust_std_origin::RustStdOriginIndex>>,
    pub(super) rust_path_owner: std::cell::OnceCell<super::rust_path_owner::RustPathOwnerIndex>,
    pub(super) rust_path_owner_project: &'a super::rust_path_owner::RustPathOwnerProjectProof,
    pub(super) rust_source_paths: &'a [Option<PathBuf>],
    pub(super) rust_path_owner_enabled: bool,
    pub(super) rust_std_project_allowed: std::cell::OnceCell<bool>,
    pub(super) rust_std_project: &'a super::rust_std_origin::RustStdProjectProof,
    pub(super) rust_cargo_reachability: &'a super::rust_cargo_reachability::RustCargoReachability,
    pub(super) python_import_cache: &'a super::python_imports::PythonImportCache,
    pub(super) python_changed_names: &'a std::collections::HashMap<String, usize>,
    pub(super) sym_to_fc: &'a [Vec<u32>],
    pub(super) file_contexts: &'a [FileContext],
    pub(super) all_symbol_names: &'a [String],
    pub(super) parent_ix_by_sym: &'a [Option<usize>],
    pub(super) pool: &'a std::sync::Mutex<StringPool>,
    /// 出力・map key 用の workspace 相対 path。
    pub(super) path_str: &'a str,
    /// ファイル読み込みや AST 判定に使う実ファイル path。
    pub(super) source_path_str: &'a str,

    pub(super) local_maps: &'a mut [TypedCallerMap],
    /// Phase 4: 低確信度 caller の振り分け先 (`WorkerState::local_low_maps` の slice)。
    pub(super) local_low_maps: &'a mut [TypedCallerMap],
    /// import-only など情報提供 caller の振り分け先。
    pub(super) local_informational_maps: &'a mut [TypedCallerMap],
    pub(super) local_def_paths: &'a mut [Vec<u32>],
    pub(super) target_cache: &'a mut LruCache<String, Option<ParsedFile>>,

    pub(super) ref_hit: &'a mut [bool],
    pub(super) ref_events: &'a mut Vec<RefEventMini>,
    pub(super) def_events: &'a mut Vec<u32>,
    /// TS/TSX/JS 名前衝突 FP 抑制用。`finish_file` 時に ref file (= `path_str`) の
    /// import specifier basename 集合を参照して high vs low routing を切り替える。
    pub(super) import_facts_cache: &'a mut LruCache<String, Option<RefFileFacts>>,
    /// `path_str` が属する workspace dir。`import_facts_cache` の構築で
    /// `dir/ref_path` を読むために使う。
    pub(super) dir: &'a str,
    /// ref file 自身の言語 (`path_str` から導出)。on_ref で都度 `LangId::from_path` するのを
    /// 避けるため、`new` 相当の初期化時にキャッシュしておく。失敗時は `None`。
    pub(super) ref_lang: Option<LangId>,
    /// 「定義を別モジュールへ移動し旧公開パスを `pub use` で維持した」変更の索引。
    /// 旧パス側に残る未変更参照を informational へ降格するために参照する。
    pub(super) reexport_moves: &'a super::reexport_move::ReexportMoveIndex,
    pub(super) ts_barrels: &'a super::ts_barrels::TsBarrelIndex,
}

impl<'a> refs::RefVisitor for ImpactCollector<'a> {
    fn on_ref(&mut self, event: refs::RefVisitEvent<'_>) {
        let refs::RefVisitEvent {
            sym_ix,
            line,
            column,
            context_column,
            context,
            is_def,
            confidence,
            rust_macro_callee,
            usage,
            lexical_binding,
            node,
            source,
        } = event;
        let ix = sym_ix as usize;
        if ix < self.ref_hit.len() {
            self.ref_hit[ix] = true;
        }
        if is_def {
            self.def_events.push(sym_ix);
            return;
        }
        if lexical_binding == LexicalBinding::FunctionLocal {
            return;
        }

        // Stage 6 (import 行) の判定は文字列のままでないと行えないため、ここで即決する。
        // caller_name も context から抽出し、pool へ intern して ID にしてから push する。
        // これにより `RefEventMini` は固定長で済み、per-file バッファの heap を削減する。
        // context は trim 済み文字列のため、列は context_column (trim 相対) を渡す
        // (絶対列 `column` は AST point 照合用で座標系が異なる)。
        let is_import = match filters::import_context_at(Some(context), context_column) {
            filters::ImportContext::NotImport => false,
            filters::ImportContext::Import => true,
            // `= require(` を含む文を丸ごと import とみなすと、同じ文の実際の呼び出し
            // (`require('./a').foo(1)`) まで import (情報提供) に落ちる。JS/TS は AST で
            // CommonJS の束縛位置かを確かめ、確かめられなければ import としない。
            filters::ImportContext::RequireOnly => {
                !self.ref_lang.is_some_and(lang_is_ts_family)
                    || node.is_some_and(|node| filters::is_commonjs_require_binding(node, source))
            }
        };
        let unproven_module_ref = is_import
            && self.ref_lang.is_some_and(lang_is_ts_family)
            && !node.is_some_and(|node| {
                node.parent().is_some_and(|parent| {
                    matches!(parent.kind(), "import_specifier" | "export_specifier")
                        && parent.child_by_field_name("alias").is_none()
                        && parent
                            .child_by_field_name("name")
                            .is_some_and(|name| name.id() == node.id())
                })
            });
        let caller_name_fallback = || self.all_symbol_names.get(ix).cloned().unwrap_or_default();
        let caller_name =
            extract_function_from_context(context).unwrap_or_else(caller_name_fallback);
        let caller_name_id = self
            .pool
            .lock()
            .expect("string pool mutex poisoned")
            .intern(&caller_name);

        // 対象のオブジェクトが無い参照では AST の追加走査も文字列確保もしない。
        let member_path = if self.ref_lang.is_some_and(lang_is_ts_family)
            && self.sym_to_fc.get(ix).is_some_and(|contexts| {
                contexts.iter().any(|&fc_ix| {
                    self.file_contexts[fc_ix as usize]
                        .object_member_changes
                        .contains_key(&self.all_symbol_names[ix])
                })
            }) {
            node.and_then(|node| super::object_members::direct_member_read(node, source))
        } else {
            None
        };
        let member_path_id = member_path.as_deref().map_or(u32::MAX, |path| {
            self.pool
                .lock()
                .expect("string pool mutex poisoned")
                .intern(path)
        });

        let confidence_u8 = match confidence {
            crate::models::reference::RefConfidence::ExactOwner => 0,
            crate::models::reference::RefConfidence::InferredOwner => 1,
            crate::models::reference::RefConfidence::BareNameOnly => 2,
        };

        // context 行で identifier がローカル binding で shadow されているか判定する。軽量 heuristic。
        // TS/TSX/JS: interface / type / const / let / var / function / class / destructured params
        // (Issue 2026-06-05-multi-attachment-conversations-fp)。
        // Rust: `let json` / `let mut json` / `for json in` (Issue 2026-06-13-ai-status-json-symbol-fp)。
        let local_shadow_hint = if self.ref_lang.is_some_and(lang_is_ts_family) {
            let symbol_name = self
                .all_symbol_names
                .get(ix)
                .map(String::as_str)
                .unwrap_or("");
            is_ts_local_shadow_context(context, symbol_name)
        } else if self.ref_lang == Some(LangId::Rust) {
            let symbol_name = self
                .all_symbol_names
                .get(ix)
                .map(String::as_str)
                .unwrap_or("");
            is_rust_local_shadow_context(context, symbol_name)
        } else {
            false
        };
        let ts_import_binding_use = self.ref_lang.is_some_and(lang_is_ts_family)
            && node.is_some_and(|node| {
                matches!(
                    node.kind(),
                    "identifier" | "type_identifier" | "shorthand_property_identifier"
                )
            });

        // Rust の証拠なし bare identifier を low に落とす際、macro 引数だけは high 維持するため
        // context 行の macro 呼び出し有無を見ておく (non-Rust では使わないので Rust のみ算出)。
        let ref_in_macro = self.ref_lang == Some(LangId::Rust)
            && context_line_has_macro_invocation(context)
            && !rust_macro_callee;
        // 行内の別マクロ (`vec![...].into_iter().collect()`) で macro hint が立っても、
        // この識別子自身が receiver の field 位置なら自由関数を指すことはない。
        // token_tree 内はマクロ展開で参照形が変わり得るため証明しない。
        let rust_receiver_member =
            self.ref_lang == Some(LangId::Rust) && node.is_some_and(is_rust_receiver_member);
        let private_method_target = self.ref_lang == Some(LangId::Rust)
            && self.sym_to_fc.get(ix).is_some_and(|contexts| {
                contexts.iter().any(|&fc_ix| {
                    self.file_contexts[fc_ix as usize]
                        .rust_private_method_owners
                        .contains_key(&self.all_symbol_names[ix])
                })
            });
        // AST ノードを持たない参照 (属性の文字列 `"super::Limits::is_unset"` など) は構文で
        // メソッドを指さないと証明できないので、メソッドを指し得る側に倒す。捨てるのは
        // 素の関数呼び出しや import のように構文で否定できた参照だけで、ノードの無い参照は
        // 部分木・葉モジュールの判定へ進める (判定できなければ低確信度)。
        let rust_method_syntax = private_method_target
            && node.is_none_or(|node| {
                is_rust_method_call_member(node)
                    || node.parent().is_some_and(|parent| {
                        parent.kind() == "scoped_identifier"
                            && parent
                                .child_by_field_name("name")
                                .is_some_and(|name| name.id() == node.id())
                    })
                    || {
                        let mut ancestor = node.parent();
                        let mut in_token_tree = false;
                        while let Some(parent) = ancestor {
                            if parent.kind() == "token_tree" {
                                in_token_tree = true;
                                break;
                            }
                            ancestor = parent.parent();
                        }
                        in_token_tree
                    }
            });
        let rust_explicit_method_owner_id = if private_method_target {
            node.and_then(|node| rust_explicit_method_owner(node, source))
                .map_or(u32::MAX, |owner| {
                    self.pool
                        .lock()
                        .expect("string pool mutex poisoned")
                        .intern(owner)
                })
        } else {
            u32::MAX
        };
        let rust_std_origin = self.ref_lang == Some(LangId::Rust)
            && node.is_some_and(|node| {
                self.rust_std_origin
                    .get_or_init(|| super::rust_std_origin::RustStdOriginIndex::build(node, source))
                    .as_ref()
                    .is_some_and(|index| index.proves_std_owner(node, source))
                    && *self
                        .rust_std_project_allowed
                        .get_or_init(|| self.rust_std_project.allows(self.source_path_str))
            });

        let python_import_owner_id = if self.ref_lang == Some(LangId::Python) {
            node.and_then(|node| {
                self.python_imports
                    .get_or_init(|| {
                        super::python_imports::PythonImportIndex::build(
                            node,
                            source,
                            self.dir,
                            self.path_str,
                            self.python_changed_names,
                            self.python_import_cache,
                        )
                    })
                    .as_ref()?
                    .owner(node, source)
            })
            .and_then(|owner| owner.to_str())
            .map_or(u32::MAX, |owner| {
                self.pool
                    .lock()
                    .expect("string pool mutex poisoned")
                    .intern(&crate::git_support::normalize_workspace_separators(owner))
            })
        } else {
            u32::MAX
        };

        let rust_owner_index = (self.ref_lang == Some(LangId::Rust)
            && self.rust_path_owner_enabled)
            .then(|| {
                node.map(|node| {
                    self.rust_path_owner.get_or_init(|| {
                        super::rust_path_owner::RustPathOwnerIndex::build(node, source)
                    })
                })
            })
            .flatten();
        let rust_owner = if let (Some(index), Some(node)) = (rust_owner_index, node) {
            let path = Path::new(self.dir);
            let type_owner = || {
                index
                    .type_owner(
                        node,
                        source,
                        path,
                        self.path_str,
                        self.rust_path_owner_project,
                    )
                    .or_else(|| {
                        super::rust_reexport_owner::owner(
                            node,
                            source,
                            path,
                            self.path_str,
                            self.rust_path_owner_project,
                            super::rust_reexport_owner::Namespace::Type,
                        )
                    })
                    .map(|owner| (owner, false))
            };
            let function_owner = || {
                index
                    .free_function_owner(
                        node,
                        source,
                        path,
                        self.path_str,
                        self.rust_path_owner_project,
                    )
                    .or_else(|| {
                        super::rust_reexport_owner::owner(
                            node,
                            source,
                            path,
                            self.path_str,
                            self.rust_path_owner_project,
                            super::rust_reexport_owner::Namespace::Function,
                        )
                    })
                    .map(|owner| (owner, true))
            };
            if is_import && self.ref_lang == Some(LangId::Rust) {
                match (type_owner(), function_owner()) {
                    (Some(owner), None) => Some(owner),
                    (None, Some(owner)) => Some(owner),
                    _ => None,
                }
            } else if node.kind() == "type_identifier" {
                type_owner()
            } else {
                function_owner()
            }
        } else {
            None
        };
        let rust_owner_path_id = rust_owner
            .as_ref()
            .and_then(|(owner, _)| owner.to_str())
            .map_or(u32::MAX, |owner| {
                self.pool
                    .lock()
                    .expect("string pool mutex poisoned")
                    .intern(owner)
            });
        let rust_owner_is_free_function = rust_owner.is_some_and(|(_, is_free)| is_free);
        let rust_alias_import_used = self.ref_lang == Some(LangId::Rust)
            && node.is_some_and(|node| {
                super::rust_reexport_owner::alias_import_used(
                    node,
                    source,
                    Path::new(self.dir),
                    self.path_str,
                    self.rust_path_owner_project,
                )
            });
        let rust_associated_type_path = rust_owner_index
            .zip(node)
            .is_some_and(|(index, node)| index.is_local_associated_path(node, source));
        let rust_plain_name = self.ref_lang == Some(LangId::Rust)
            && !ref_in_macro
            && !rust_macro_callee
            && node.is_some_and(|node| {
                matches!(node.kind(), "identifier" | "type_identifier")
                    && node.parent().is_none_or(|parent| {
                        !matches!(
                            parent.kind(),
                            "scoped_identifier" | "scoped_type_identifier"
                        )
                    })
            });
        let rust_local_owner = rust_plain_name
            && rust_owner_index.zip(node).is_some_and(|(index, node)| {
                index.plain_local_owner(node, source, &self.all_symbol_names[ix])
                    || index.plain_local_function_owner(node, source, &self.all_symbol_names[ix])
            });
        let rust_direct_import_site = rust_owner_index.zip(node).is_some_and(|(index, node)| {
            index.is_tracked_direct_import_site(node, source, &self.all_symbol_names[ix])
        });
        let rust_import_binding_safe = rust_plain_name
            && rust_owner_index.zip(node).is_some_and(|(index, node)| {
                index.plain_import_binding_safe(node, source, &self.all_symbol_names[ix])
            });

        let jsx_shape_id = if self.ref_lang == Some(LangId::Tsx) {
            node.and_then(|node| {
                let clean = *self
                    .jsx_parse_clean
                    .get_or_init(|| super::jsx_props::parse_is_clean(node));
                super::jsx_props::caller_shape(node, source, clean)
            })
            .and_then(|shape| {
                let id = u32::try_from(self.jsx_shapes.len()).ok()?;
                self.jsx_shapes.push(shape);
                Some(id)
            })
            .unwrap_or(u32::MAX)
        } else {
            u32::MAX
        };
        self.ref_events.push(RefEventMini {
            sym_ix,
            line: line as u32,
            column: column as u32,
            caller_name_id,
            is_import,
            rust_alias_import_used,
            confidence: confidence_u8,
            local_shadow_hint,
            ts_import_binding_use,
            ref_in_macro,
            rust_macro_callee,
            rust_receiver_member,
            rust_method_syntax,
            rust_explicit_method_owner_id,
            rust_std_origin,
            fn_value_ref: usage == refs::RefUsageRole::FunctionValue,
            member_path_id,
            jsx_shape_id,
            unproven_module_ref,
            rust_value_binding: lexical_binding == LexicalBinding::RustValueBinding,
            python_import_owner_id,
            rust_owner_path_id,
            rust_owner_is_free_function,
            rust_associated_type_path,
            rust_local_owner,
            rust_direct_import_site,
            rust_import_binding_safe,
        });
    }
}

/// 同名の別 kind をまとめて除外しない。変更後 kind も含めて全件を確認する。
fn rust_function_only(context: &FileContext, name: &str) -> bool {
    context.lang_id == LangId::Rust && context.rust_function_names.contains(name)
}

fn is_rust_receiver_member(node: tree_sitter::Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if parent.kind() != "field_expression"
        || !parent
            .child_by_field_name("field")
            .is_some_and(|field| field.id() == node.id())
    {
        return false;
    }
    let mut ancestor = Some(parent);
    while let Some(current) = ancestor {
        if current.kind() == "token_tree" {
            return false;
        }
        ancestor = current.parent();
    }
    true
}

fn is_rust_method_call_member(node: tree_sitter::Node<'_>) -> bool {
    if !is_rust_receiver_member(node) {
        return false;
    }
    let Some(field) = node.parent() else {
        return false;
    };
    let callee = field
        .parent()
        .filter(|generic| {
            generic.kind() == "generic_function"
                && generic
                    .child_by_field_name("function")
                    .is_some_and(|function| function.id() == field.id())
        })
        .unwrap_or(field);
    callee.parent().is_some_and(|call| {
        call.kind() == "call_expression"
            && call
                .child_by_field_name("function")
                .is_some_and(|function| function.id() == callee.id())
    })
}

fn enclosing_rust_impl_owner<'a>(node: tree_sitter::Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    let mut ancestor = node.parent();
    while let Some(parent) = ancestor {
        if parent.kind() == "impl_item" {
            let owner = parent.child_by_field_name("type")?;
            let owner = if owner.kind() == "generic_type" {
                owner.named_child(0)?
            } else {
                owner
            };
            return (owner.kind() == "type_identifier")
                .then(|| owner.utf8_text(source).ok())
                .flatten();
        }
        ancestor = parent.parent();
    }
    None
}

/// A written receiver type is only a hint: aliases can refer to the changed type.
/// Use a mismatch to lower confidence, never to erase the reference.
fn rust_explicit_method_owner<'a>(
    node: tree_sitter::Node<'_>,
    source: &'a [u8],
) -> Option<&'a str> {
    let parent = node.parent()?;
    if is_rust_receiver_member(node) {
        let receiver = parent.child_by_field_name("value")?;
        return (receiver.utf8_text(source).ok()? == "self")
            .then(|| enclosing_rust_impl_owner(node, source))
            .flatten();
    }
    if parent.kind() != "scoped_identifier"
        || !parent
            .child_by_field_name("name")
            .is_some_and(|name| name.id() == node.id())
    {
        return None;
    }
    let path = parent.child_by_field_name("path")?;
    let owner = match path.kind() {
        "identifier" | "type_identifier" => path,
        "scoped_identifier" => path.child_by_field_name("name")?,
        _ => return None,
    };
    let owner = owner.utf8_text(source).ok()?;
    if owner == "Self" {
        enclosing_rust_impl_owner(node, source)
    } else {
        Some(owner)
    }
}

/// 参照 1 件 × 変更元 1 件 (候補) の判定結果。
enum CandidateDecision {
    /// 別の所有元・スコープの外・テストの中などと証明できたので caller にしない。
    Exclude,
    /// caller として残す。値は振り分け先。
    Keep(CallerRoute),
}

/// `finish_file` の間だけ使う、参照ファイル単位の証拠とキャッシュ。
struct RefFileEvidence<'a> {
    filter_disabled: bool,
    /// 変更元ごとに、Cargo の依存関係で参照ファイルから届かないと証明できたか。
    rust_cargo_unreachable: Vec<bool>,
    /// 変更元ごとの Python ソースの実体パス (symlink を解決済み)。
    python_source_paths: &'a [Option<String>],
    rust_owner_paths: HashMap<u32, Option<PathBuf>>,
    /// 変更元ごとに、参照ファイルが旧 private 宣言のモジュール部分木に入るか。
    /// `None` は証明できないことを表し、`Some(false)` とは区別する。
    rust_private_scope: HashMap<usize, Option<bool>>,
    rust_private_import_owners: HashMap<(usize, usize), bool>,
    rust_ref_source: std::cell::OnceCell<Option<Vec<u8>>>,
}

/// 参照 1 件について、変更元に依らない事実。
struct EventFacts {
    python_owner: Option<String>,
    has_parent_type: bool,
    parent_in_this_file: bool,
    base_route_low: bool,
}

/// 除外しなかった候補の振り分けに使う、Rust の private スコープの証拠。
struct ScopeEvidence {
    /// 可視性を広げただけの変更で、旧 private 部分木の中の参照と証明できた。
    visibility_only_informational: bool,
    private_method_candidate: bool,
    /// private メソッドの所有型・部分木を証明できなかった。
    private_method_low: bool,
}

impl<'a> ImpactCollector<'a> {
    /// ファイル走査完了時に呼ぶ。buffered events に対して Stage 1-6 (Stage 4b 除く) の
    /// filter を適用し、`local_maps` / `local_def_paths` に push する。バッファは clear して
    /// 次ファイルで再利用する。
    ///
    /// 定義の記録 → 参照ファイル単位の証拠の準備 → 参照 × 変更元ごとの判定
    /// (`candidate_decision`) → 振り分け先への集約 (`record_caller`) の順に行う。
    pub(super) fn finish_file(self) {
        let mut collector = self;
        collector.record_definitions();

        // References: 1 件ずつ Stage 1-6 (Stage 4b 除く) の filter を適用し local_maps へ流す。
        // Stage 6 (import 判定) と caller_name の抽出は on_ref 時点で済ませてあるため、
        // ここでは flag / ID で判定する。Phase 4: confidence == BareNameOnly + シンボル名が
        // generic (new/update/...) なら local_low_maps へ振り分け、強い impact 信号を汚染しない。
        // バッファは取り出して drain し、ヒープを次のファイルで再利用するため戻す。
        let mut events = std::mem::take(&mut *collector.ref_events);
        let mut evidence = collector.prepare_file_evidence(&events);
        let sym_to_fc = collector.sym_to_fc;
        for e in events.drain(..) {
            let fc_ixs = &sym_to_fc[e.sym_ix as usize];
            if fc_ixs.is_empty() {
                continue;
            }
            let facts = collector.event_facts(&e, evidence.filter_disabled);
            for &fc_ix_raw in fc_ixs {
                let fc_ix = fc_ix_raw as usize;
                if let CandidateDecision::Keep(route) =
                    collector.candidate_decision(&e, &facts, fc_ix, &mut evidence)
                {
                    collector.record_caller(&e, fc_ix, route);
                }
            }
        }
        *collector.ref_events = events;

        // ref_hit のクリア（次ファイル向け）
        for v in collector.ref_hit.iter_mut() {
            *v = false;
        }
    }

    /// Definition: path を 1 回だけ intern して全 def sym へ配布する。
    fn record_definitions(&mut self) {
        if self.def_events.is_empty() {
            return;
        }
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

    /// 参照ファイル単位で 1 回だけ求める証拠と、候補の判定で共有するキャッシュを用意する。
    fn prepare_file_evidence(&self, events: &[RefEventMini]) -> RefFileEvidence<'a> {
        let filter_disabled = confidence_filter_disabled();
        let rust_cargo_unreachable = if self.ref_lang == Some(LangId::Rust) && !events.is_empty() {
            self.file_contexts
                .iter()
                .map(|context| {
                    context.lang_id == LangId::Rust
                        && self
                            .rust_cargo_reachability
                            .proves_unreachable(self.path_str, &context.new_path)
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        // symlink 経由の同一ファイルを別の所有元と判定しない。変更元ごとに1回だけ解決する。
        let python_import_cache = self.python_import_cache;
        let python_source_paths = if events
            .iter()
            .any(|event| event.python_import_owner_id != u32::MAX)
        {
            python_import_cache
                .source_paths
                .get_or_init(|| {
                    self.file_contexts
                        .iter()
                        .map(|context| {
                            (context.lang_id == LangId::Python
                                && !camino::Utf8Path::new(&context.new_path)
                                    .extension()
                                    .is_some_and(|extension| extension.eq_ignore_ascii_case("pyi")))
                            .then(|| {
                                std::path::Path::new(self.dir)
                                    .join(&context.new_path)
                                    .canonicalize()
                                    .ok()
                                    .and_then(|path| {
                                        path.to_str()
                                            .map(crate::git_support::normalize_workspace_separators)
                                    })
                            })
                            .flatten()
                        })
                        .collect()
                })
                .as_slice()
        } else {
            &[]
        };
        RefFileEvidence {
            filter_disabled,
            rust_cargo_unreachable,
            python_source_paths,
            rust_owner_paths: HashMap::new(),
            rust_private_scope: HashMap::new(),
            rust_private_import_owners: HashMap::new(),
            rust_ref_source: std::cell::OnceCell::new(),
        }
    }

    /// 参照 1 件について、変更元に依らない事実を求める。
    fn event_facts(&self, e: &RefEventMini, filter_disabled: bool) -> EventFacts {
        let sym_ix_usize = e.sym_ix as usize;
        let python_owner = (e.python_import_owner_id != u32::MAX).then(|| {
            self.pool
                .lock()
                .expect("string pool mutex poisoned")
                .get(e.python_import_owner_id)
                .to_owned()
        });
        let has_parent_type = self.parent_ix_by_sym[sym_ix_usize].is_some();
        let parent_in_this_file = self.parent_ix_by_sym[sym_ix_usize]
            .and_then(|pix| self.ref_hit.get(pix))
            .copied()
            .unwrap_or(false);
        let base_route_low = !filter_disabled && is_low_confidence_caller(e, self.all_symbol_names);
        EventFacts {
            python_owner,
            has_parent_type,
            parent_in_this_file,
            base_route_low,
        }
    }

    /// 参照 `e` を変更元 `fc_ix` の caller にするか、するならどこへ振り分けるかを決める。
    ///
    /// 除外の証拠を所有元 → private スコープ → 参照位置の順に確かめ、残った候補だけ振り分け先を
    /// 決める。除外には confidence filter を無効にしても効くものと止まるものが混在するので、
    /// 各証拠の中の `filter_disabled` の条件は個別に保つ。
    fn candidate_decision(
        &mut self,
        e: &RefEventMini,
        facts: &EventFacts,
        fc_ix: usize,
        evidence: &mut RefFileEvidence<'a>,
    ) -> CandidateDecision {
        if self.excluded_by_owner_evidence(e, facts, fc_ix, evidence) {
            return CandidateDecision::Exclude;
        }
        let Some(scope) = self.private_scope_evidence(e, facts, fc_ix, evidence) else {
            return CandidateDecision::Exclude;
        };
        if self.excluded_by_reference_site(e, facts, fc_ix, scope.private_method_candidate) {
            return CandidateDecision::Exclude;
        }
        CandidateDecision::Keep(self.route(e, facts, fc_ix, &scope, evidence.filter_disabled))
    }

    /// 参照が別の所有元を指すと証明できたか (Python の import 先、Rust の値束縛・
    /// receiver member・std 起点・所有元のパス・Cargo の到達性)。
    fn excluded_by_owner_evidence(
        &self,
        e: &RefEventMini,
        facts: &EventFacts,
        fc_ix: usize,
        evidence: &mut RefFileEvidence<'a>,
    ) -> bool {
        let file_contexts = self.file_contexts;
        let ctx = &file_contexts[fc_ix];
        let all_symbol_names = self.all_symbol_names;
        let name = &all_symbol_names[e.sym_ix as usize];
        let filter_disabled = evidence.filter_disabled;
        if let Some(owner) = facts.python_owner.as_deref()
            && let Some(Some(source_path)) = evidence.python_source_paths.get(fc_ix)
            && owner != source_path
        {
            return true;
        }
        if e.rust_value_binding && rust_function_only(ctx, name) {
            return true;
        }
        if e.rust_receiver_member && ctx.rust_free_function_names.contains(name) {
            return true;
        }
        if e.rust_std_origin && ctx.lang_id == LangId::Rust {
            return true;
        }
        if e.rust_associated_type_path
            && ctx.lang_id == LangId::Rust
            && !filter_disabled
            && ctx.rust_free_function_names.contains(name)
        {
            return true;
        }
        if e.rust_owner_path_id != u32::MAX
            && ctx.lang_id == LangId::Rust
            && !filter_disabled
            && (e.rust_owner_is_free_function == ctx.rust_free_function_names.contains(name))
        {
            let owner = evidence
                .rust_owner_paths
                .entry(e.rust_owner_path_id)
                .or_insert_with(|| {
                    let owner = {
                        let pool = self.pool.lock().expect("string pool mutex poisoned");
                        pool.get(e.rust_owner_path_id).to_owned()
                    };
                    Path::new(self.dir).join(owner).canonicalize().ok()
                });
            if let (Some(owner), Some(Some(source))) =
                (owner.as_ref(), self.rust_source_paths.get(fc_ix))
                && owner != source
            {
                return true;
            }
        }
        evidence.rust_cargo_unreachable.get(fc_ix) == Some(&true)
    }

    /// Rust の private 宣言のスコープの証拠。参照が旧 private 部分木の外・葉モジュールの外・
    /// 別の所有元の import などと証明できたら `None` (除外)。残すなら振り分けに使う証拠を返す。
    fn private_scope_evidence(
        &self,
        e: &RefEventMini,
        facts: &EventFacts,
        fc_ix: usize,
        evidence: &mut RefFileEvidence<'a>,
    ) -> Option<ScopeEvidence> {
        let file_contexts = self.file_contexts;
        let ctx = &file_contexts[fc_ix];
        let all_symbol_names = self.all_symbol_names;
        let sym_ix_usize = e.sym_ix as usize;
        let name = &all_symbol_names[sym_ix_usize];
        let filter_disabled = evidence.filter_disabled;
        let source_path = &ctx.new_path;
        let visibility_only_widening = ctx.lang_id == LangId::Rust
            && !filter_disabled
            && !filters::is_same_source_file(self.path_str, source_path)
            && ctx.rust_private_widening_names.contains(name)
            && self.rust_path_owner_project.allows_private_widening_paths();
        let visibility_only_informational = if visibility_only_widening {
            let old_scope = *evidence
                .rust_private_scope
                .entry(fc_ix)
                .or_insert_with(|| self.private_subtree_contains(source_path));
            if old_scope == Some(false) {
                return None;
            }
            old_scope == Some(true)
        } else {
            false
        };
        if ctx.lang_id == LangId::Rust
            && !filter_disabled
            && !filters::is_same_source_file(self.path_str, source_path)
            && ctx.rust_private_root_names.contains(name)
        {
            if self
                .rust_path_owner_project
                .private_source_is_leaf(source_path)
            {
                return None;
            }
            if e.rust_local_owner {
                return None;
            }
            let outside_subtree = evidence
                .rust_private_scope
                .entry(fc_ix)
                .or_insert_with(|| self.private_subtree_contains(source_path))
                == &Some(false);
            if outside_subtree {
                return None;
            }
            let rust_ref_source = &evidence.rust_ref_source;
            let other_import = (e.rust_import_binding_safe || e.rust_direct_import_site)
                && *evidence
                    .rust_private_import_owners
                    .entry((fc_ix, sym_ix_usize))
                    .or_insert_with(|| {
                        self.rust_path_owner.get().is_some_and(|index| {
                            rust_ref_source
                                .get_or_init(|| {
                                    crate::engine::bounded_read::read_input_file(
                                        &Path::new(self.dir).join(self.path_str),
                                    )
                                })
                                .as_ref()
                                .is_some_and(|source| {
                                    index.imports_other_private_owner(
                                        Path::new(self.dir),
                                        source_path,
                                        self.path_str,
                                        source,
                                        name,
                                        self.rust_path_owner_project,
                                    )
                                })
                        })
                    });
            if other_import {
                return None;
            }
        }
        let private_method_candidate = ctx.lang_id == LangId::Rust
            && !filter_disabled
            && ctx.rust_private_method_owners.contains_key(name);
        if private_method_candidate && (!e.rust_method_syntax || e.is_import) {
            return None;
        }
        let private_method_owner_mismatch = private_method_candidate
            && e.rust_explicit_method_owner_id != u32::MAX
            && ctx
                .rust_private_method_owners
                .get(name)
                .is_some_and(|owner| {
                    let pool = self.pool.lock().expect("string pool mutex poisoned");
                    pool.get(e.rust_explicit_method_owner_id) != owner
                });
        let private_method_unanchored = private_method_candidate
            && facts.has_parent_type
            && !facts.parent_in_this_file
            && e.rust_explicit_method_owner_id == u32::MAX;
        let mut private_method_scope_uncertain = false;
        if private_method_candidate && !filters::is_same_source_file(self.path_str, source_path) {
            if self
                .rust_path_owner_project
                .private_source_is_leaf(source_path)
            {
                return None;
            }
            let subtree = *evidence
                .rust_private_scope
                .entry(fc_ix)
                .or_insert_with(|| self.private_subtree_contains(source_path));
            if subtree == Some(false) {
                return None;
            }
            private_method_scope_uncertain = subtree.is_none();
        }
        Some(ScopeEvidence {
            visibility_only_informational,
            private_method_candidate,
            private_method_low: private_method_scope_uncertain
                || private_method_owner_mismatch
                || private_method_unanchored,
        })
    }

    /// 参照ファイルが変更元の旧 private 宣言のモジュール部分木に入るか (`None` は証明できない)。
    fn private_subtree_contains(&self, source_path: &str) -> Option<bool> {
        self.rust_path_owner.get().and_then(|index| {
            index.private_subtree_contains(
                Path::new(self.dir),
                source_path,
                self.path_str,
                self.rust_path_owner_project,
            )
        })
    }

    /// 参照の位置から caller にならないと分かるか (変更元と同じファイル、言語の系統が違う、
    /// 親型の参照がこのファイルに無い、テストの中)。
    fn excluded_by_reference_site(
        &mut self,
        e: &RefEventMini,
        facts: &EventFacts,
        fc_ix: usize,
        private_method_candidate: bool,
    ) -> bool {
        let file_contexts = self.file_contexts;
        let ctx = &file_contexts[fc_ix];
        let source_lang_group = lang_compat_group(ctx.lang_id);
        if filters::is_same_source_file(self.path_str, &ctx.new_path) {
            return true;
        }
        // ref file の言語は `self.ref_lang` に初期化時キャッシュ済み。
        // ホットループ内で `LangId::from_path` を都度再導出しない。
        if let Some(ref_lang) = self.ref_lang
            && lang_compat_group(ref_lang) != source_lang_group
        {
            return true;
        }
        if facts.has_parent_type && !facts.parent_in_this_file && !private_method_candidate {
            return true;
        }
        is_ref_in_target_test_context(
            self.source_path_str,
            e.line as usize,
            e.column as usize,
            self.target_cache,
        )
    }

    /// 除外しなかった候補の振り分け先。外側の優先順位 (可視性拡大の informational →
    /// private メソッドの Low) を先に見て、残りを `RouteDecisionContext::decide`
    /// (informational → Low → 不変メンバーの informational → Normal) に任せる。
    fn route(
        &mut self,
        e: &RefEventMini,
        facts: &EventFacts,
        fc_ix: usize,
        scope: &ScopeEvidence,
        filter_disabled: bool,
    ) -> CallerRoute {
        let file_contexts = self.file_contexts;
        let ctx = &file_contexts[fc_ix];
        let all_symbol_names = self.all_symbol_names;
        let sym_key_canonical = &all_symbol_names[e.sym_ix as usize];
        // pool の lock 中には比較だけを行い、import 解析前に解放する。
        let member_unchanged = (e.member_path_id != u32::MAX
            && ctx
                .object_member_changes
                .get(sym_key_canonical)
                .is_some_and(|change| {
                    let pool = self.pool.lock().expect("string pool mutex poisoned");
                    change.read_is_unchanged(pool.get(e.member_path_id))
                }))
            || self
                .jsx_shapes
                .get(e.jsx_shape_id as usize)
                .is_some_and(|shape| {
                    ctx.jsx_prop_changes
                        .get(sym_key_canonical)
                        .is_some_and(|change| change.is_omitted_by(shape))
                });
        if scope.visibility_only_informational {
            CallerRoute::Informational
        } else if scope.private_method_low {
            CallerRoute::Low
        } else {
            RouteDecisionContext {
                filter_disabled,
                ref_lang: self.ref_lang,
                dir: self.dir,
                path_str: self.path_str,
                import_facts_cache: &mut *self.import_facts_cache,
                reexport_moves: self.reexport_moves,
                ts_barrels: self.ts_barrels,
            }
            .decide(
                e,
                ctx,
                sym_key_canonical,
                facts.has_parent_type,
                facts.base_route_low,
                member_unchanged,
            )
        }
    }

    /// 残した候補を振り分け先の map へ足す。同じ行では最初の caller 名を保ち、
    /// 同じ (シンボル, 表示名) の組は 1 回だけ足す。
    fn record_caller(&mut self, e: &RefEventMini, fc_ix: usize, route: CallerRoute) {
        let file_contexts = self.file_contexts;
        let ctx = &file_contexts[fc_ix];
        let all_symbol_names = self.all_symbol_names;
        let sym_key_canonical = &all_symbol_names[e.sym_ix as usize];
        // 事前 index (ci_key→元名) で O(1) 参照。per-ref の線形 find + ci_key String 割当を排除。
        let affected_sym_name = ctx
            .affected_name_by_cikey
            .get(sym_key_canonical.as_str())
            .cloned()
            .unwrap_or_else(|| sym_key_canonical.clone());

        let (path_id, sym_name_id) = {
            let mut p = self.pool.lock().expect("string pool mutex poisoned");
            (p.intern(self.path_str), p.intern(&affected_sym_name))
        };

        let ix_u32 = e.sym_ix;
        let key = (path_id, e.line as usize);
        let target_map = match route {
            CallerRoute::Normal => &mut self.local_maps[fc_ix],
            CallerRoute::Low => &mut self.local_low_maps[fc_ix],
            CallerRoute::Informational => &mut self.local_informational_maps[fc_ix],
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
    use crate::models::impact::{AffectedSymbol, SignatureChange};

    fn ev(sym_ix: u32, confidence: u8) -> RefEventMini {
        RefEventMini {
            sym_ix,
            line: 0,
            column: 0,
            caller_name_id: 0,
            is_import: false,
            rust_alias_import_used: false,
            confidence,
            local_shadow_hint: false,
            ts_import_binding_use: false,
            ref_in_macro: false,
            rust_macro_callee: false,
            rust_receiver_member: false,
            rust_method_syntax: false,
            rust_explicit_method_owner_id: u32::MAX,
            rust_std_origin: false,
            fn_value_ref: false,
            member_path_id: u32::MAX,
            jsx_shape_id: u32::MAX,
            unproven_module_ref: false,
            rust_value_binding: false,
            python_import_owner_id: u32::MAX,
            rust_owner_path_id: u32::MAX,
            rust_owner_is_free_function: false,
            rust_associated_type_path: false,
            rust_local_owner: false,
            rust_direct_import_site: false,
            rust_import_binding_safe: false,
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

    fn file_context(
        affected: Vec<AffectedSymbol>,
        sig_changes: Vec<SignatureChange>,
    ) -> FileContext {
        FileContext {
            new_path: "src/changed.rs".to_string(),
            lang_id: LangId::Rust,
            rust_function_names: super::super::rust_function_names(LangId::Rust, &affected),
            rust_free_function_names: Default::default(),
            affected,
            sig_changes,
            compatible_signature_keys: Default::default(),
            hunks: Vec::new(),
            call_edges: Vec::new(),
            cross_file_symbol_keys: Default::default(),
            affected_name_by_cikey: Default::default(),
            object_member_changes: Default::default(),
            jsx_prop_changes: Default::default(),
            rust_kind_changes: Default::default(),
            rust_private_root_names: Default::default(),
            rust_private_method_owners: Default::default(),
            rust_private_widening_names: Default::default(),
        }
    }

    #[test]
    fn rust_scope_kind_proof_is_per_context_and_requires_all_same_named_kinds() {
        let symbol = |kind: &str| AffectedSymbol {
            name: "comments".to_owned(),
            kind: kind.to_owned(),
            change_type: "modified".to_owned(),
        };
        let functions = file_context(vec![symbol("function"), symbol("method")], Vec::new());
        let constants = file_context(vec![symbol("constant")], Vec::new());
        let mixed = file_context(vec![symbol("function"), symbol("constant")], Vec::new());
        assert!(rust_function_only(&functions, "comments"));
        assert!(!rust_function_only(&constants, "comments"));
        assert!(!rust_function_only(&mixed, "comments"));
        assert!(!rust_function_only(&functions, "unknown"));
        assert!(!rust_function_only(
            &file_context(Vec::new(), Vec::new()),
            "comments"
        ));
        for kind in ["struct", "enum", "type", "variable"] {
            assert!(!rust_function_only(
                &file_context(vec![symbol(kind)], Vec::new()),
                "comments"
            ));
        }
    }

    fn decide_route(event: &RefEventMini, context: &FileContext, base_low: bool) -> CallerRoute {
        let mut cache = LruCache::unbounded();
        // 再輸出移動の索引は空 = 降格なし。本テストは3バケットの優先順位だけを固定する
        // (再輸出移動の判定は `reexport_move` のユニットテストと E2E が担当)。
        let reexport_moves = super::super::reexport_move::ReexportMoveIndex::build("");
        let ts_barrels = super::super::ts_barrels::TsBarrelIndex::default();
        RouteDecisionContext {
            // TS/Rust のファイル解析を伴う補正は既存の import_facts テストに任せ、
            // ここでは3バケットの優先順位だけを固定する。
            filter_disabled: true,
            ref_lang: None,
            dir: ".",
            path_str: "src/caller.rs",
            import_facts_cache: &mut cache,
            reexport_moves: &reexport_moves,
            ts_barrels: &ts_barrels,
        }
        .decide(event, context, "target", false, base_low, false)
    }

    /// informational は low より優先し、import は modified 以外を low に落とさない。
    #[test]
    fn route_decision_preserves_bucket_priority_and_import_boundary() {
        let empty = file_context(Vec::new(), Vec::new());
        assert_eq!(decide_route(&ev(0, 0), &empty, false), CallerRoute::Normal);
        assert_eq!(decide_route(&ev(0, 0), &empty, true), CallerRoute::Low);

        let mut import = ev(0, 0);
        import.is_import = true;
        assert_eq!(decide_route(&import, &empty, true), CallerRoute::Normal);

        let modified = file_context(
            vec![AffectedSymbol {
                name: "target".to_string(),
                kind: "function".to_string(),
                change_type: "modified".to_string(),
            }],
            Vec::new(),
        );
        assert_eq!(
            decide_route(&import, &modified, true),
            CallerRoute::Informational
        );
        import.unproven_module_ref = true;
        assert_eq!(
            decide_route(&import, &modified, true),
            CallerRoute::Informational
        );

        let mut function_value = ev(0, 0);
        function_value.fn_value_ref = true;
        let same_arity = file_context(
            Vec::new(),
            vec![SignatureChange {
                name: "target".to_string(),
                old_signature: "fn target(a: i32)".to_string(),
                new_signature: "fn target(a: i64)".to_string(),
            }],
        );
        assert_eq!(
            decide_route(&function_value, &same_arity, true),
            CallerRoute::Informational
        );

        let mut changed_kind = same_arity;
        changed_kind.rust_kind_changes.insert("target".to_owned());
        assert_eq!(
            decide_route(&function_value, &changed_kind, false),
            CallerRoute::Normal
        );

        let changed_arity = file_context(
            Vec::new(),
            vec![SignatureChange {
                name: "target".to_string(),
                old_signature: "fn target(a: i32)".to_string(),
                new_signature: "fn target(a: i32, b: i32)".to_string(),
            }],
        );
        assert_eq!(
            decide_route(&function_value, &changed_arity, false),
            CallerRoute::Normal
        );

        let mut transparent_alias = file_context(Vec::new(), Vec::new());
        transparent_alias
            .compatible_signature_keys
            .insert("target".to_string());
        assert_eq!(
            decide_route(&ev(0, 0), &transparent_alias, true),
            CallerRoute::Informational,
            "証明済み alias 導入は low/high より優先して informational に残す"
        );
    }
}
