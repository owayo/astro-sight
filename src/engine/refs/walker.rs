//! 統合参照ウォーカーと、その照合戦略 (matcher) / 出力先 (sink)。
//!
//! 単一名検索・バッチ検索・visitor callback・件数カウントの 4 経路を `walk_refs`
//! 1 本に集約する。照合差は [`RefMatcher`]、出力差は [`RawRefSink`] の実装に閉じ込め、
//! ref 源を追加するときは `walk_refs` 1 箇所だけを編集すればよい。

use tree_sitter::Node;

use crate::engine::bash_trap_refs::bash_trap_handler_ref_segments;
use crate::engine::phpunit_refs::phpunit_metadata_ref_segments;
use crate::language::{LangId, normalize_identifier};
use crate::models::reference::{RefConfidence, RefKind, SymbolReference};

use super::definition::php::php_name_is_case_insensitive;
use super::definition::php::{
    php_callable_array_method_segment, php_string_callable_method_segment,
};
use super::definition::rust::{
    RustPatternBindingCache, is_rust_closure_bound_identifier, is_rust_struct_field_non_callable,
    rust_attr_string_ref_segments,
};
use super::definition::{is_definition_context, is_identifier_kind, is_ignored_identifier_context};
use super::line_index::{LineIndex, context_column, extract_line_context_indexed};
use super::role::{
    RefUsageRole, classify_method_ref_confidence, classify_ref_usage_role,
    is_rust_macro_invocation_callee,
};

/// 参照走査で name_to_ix を引くマッチキーを生成する。
/// PHP の関数/メソッド/クラス系の名前は case-insensitive に折りたたむ。
fn node_ref_key<'a>(lang_id: LangId, node: Node<'_>, text: &'a str) -> std::borrow::Cow<'a, str> {
    if lang_id == LangId::Php && php_name_is_case_insensitive(node) {
        return std::borrow::Cow::Owned(text.to_ascii_lowercase());
    }
    normalize_identifier(lang_id, text)
}

/// 文字列セグメント由来の参照 (PHPUnit metadata / callable array / `'Class@method'`) の
/// マッチキーを生成する。PHP のこれらのセグメントは常にメソッド名なので case-insensitive
/// に折りたたむ。Rust/Bash のセグメントは従来どおり normalize_identifier に従う。
fn seg_ref_key<'a>(lang_id: LangId, seg: &'a str) -> std::borrow::Cow<'a, str> {
    if lang_id == LangId::Php {
        return std::borrow::Cow::Owned(seg.to_ascii_lowercase());
    }
    normalize_identifier(lang_id, seg)
}

/// present_indices のシンボルから name_to_ix を構築する。
///
/// PHP では case-insensitive な名前 (関数/メソッド/クラス系) の参照に備え、元キーに加えて
/// 小文字化キーも登録する (folded == exact の場合は重複登録しない)。1 つの参照ノードは
/// `node_ref_key` で生成した単一キーのみを引くため、二重登録によるカウント二重化は起きない。
pub(crate) fn build_name_to_ix<'a>(
    lang_id: LangId,
    symbol_names: &'a [String],
    present_indices: &std::collections::HashSet<usize>,
) -> std::collections::HashMap<std::borrow::Cow<'a, str>, Vec<usize>> {
    use std::borrow::Cow;
    let mut map: std::collections::HashMap<Cow<'a, str>, Vec<usize>> =
        std::collections::HashMap::with_capacity(present_indices.len());
    for &i in present_indices {
        let raw = symbol_names[i].as_str();
        if lang_id == LangId::Php {
            let folded = raw.to_ascii_lowercase();
            if folded != raw {
                map.entry(Cow::Owned(folded)).or_default().push(i);
            }
        }
        let key = normalize_identifier(lang_id, raw);
        map.entry(key).or_default().push(i);
    }
    map
}

/// 単一参照検索で identifier ノードのテキストが target に一致するか判定する。
/// PHP の case-insensitive 文脈では大小無視で比較する。`target` は呼び出し側で
/// `normalize_identifier` 済み (PHP では原文のまま) を前提とする。
fn ident_ref_matches(lang_id: LangId, node: Node<'_>, text: &str, target: &str) -> bool {
    if lang_id == LangId::Php && php_name_is_case_insensitive(node) {
        text.eq_ignore_ascii_case(target)
    } else {
        normalize_identifier(lang_id, text).as_ref() == target
    }
}

/// 単一参照検索で文字列セグメント (PHP method 名等) が target に一致するか判定する。
fn seg_ref_matches(lang_id: LangId, seg: &str, target: &str) -> bool {
    if lang_id == LangId::Php {
        seg.eq_ignore_ascii_case(target)
    } else {
        normalize_identifier(lang_id, seg).as_ref() == target
    }
}

/// `visit_refs_and_defs_in_file_cb` が visitor に渡す最小参照イベント。
pub(crate) struct RefVisitEvent<'a> {
    pub(crate) sym_ix: u32,
    pub(crate) line: usize,
    /// ファイル絶対 byte 列 (tree-sitter Point と同じ座標系)。
    /// AST の point 照合 (test context 判定等) にはこちらを使う。
    pub(crate) column: usize,
    /// trim 済み `context` 行内の相対列。`context` 文字列に対する
    /// statement 単位の判定 (import/re-export 分類等) にはこちらを使う。
    pub(crate) context_column: usize,
    pub(crate) context: &'a str,
    pub(crate) is_def: bool,
    /// receiver-aware 解析の確信度。Phase 3 より前は常に ExactOwner。
    pub(crate) confidence: RefConfidence,
    /// Rust macro invocation の callee identifier かどうか。
    pub(crate) rust_macro_callee: bool,
    /// 参照の AST 上の使われ方 (identifier 経路のみ分類、他経路は `Other`)。
    pub(crate) usage: RefUsageRole,
}

/// `visit_refs_and_defs_in_file_cb` が内部で呼び出す訪問者 trait。
/// Xojo の case-insensitive 多重 index (`name_to_ix[key]` が `Vec<usize>`) や
/// Rust attribute 文字列内参照の場合も、ヒットしたすべての sym_ix について
/// 1 回ずつ `on_ref` が呼ばれる。
pub(crate) trait RefVisitor {
    fn on_ref(&mut self, event: RefVisitEvent<'_>);
}

// ---------------------------------------------------------------------------
// 統合参照ウォーカー
//
// 従来 4 本 (collect_identifier_refs / collect_refs_and_defs_indexed_cb /
// collect_identifier_refs_indexed / count_identifier_refs) にコピペされていた
// 「identifier ガード + 5 種 synthetic ref 源抽出 + 子への再帰」を 1 本の
// `walk_refs` に集約する。出力差 (単一 Vec / index 別 Vec / callback / カウント) は
// `RawRefSink` 実装に、単一名照合と index 照合の差は `RefMatcher` 実装に閉じ込める。
// ref 源を追加するときは walk_refs 1 箇所だけを編集すればよい。
// ---------------------------------------------------------------------------

/// 1 hit で一致した「シンボル index 集合」。単一名検索は借用を生まない `One`、
/// batch 検索は `name_to_ix` の `Vec<usize>` を借用する `Many`。
pub(crate) enum MatchSet<'idx> {
    One(usize),
    Many(&'idx [usize]),
}

impl MatchSet<'_> {
    /// 一致した全 index に対して f を呼ぶ (One は 1 回、Many は要素数分)。
    #[inline]
    fn for_each_index(&self, mut f: impl FnMut(usize)) {
        match *self {
            MatchSet::One(ix) => f(ix),
            MatchSet::Many(ixs) => {
                for &ix in ixs {
                    f(ix);
                }
            }
        }
    }
}

/// hit の発生源。identifier ノード由来か、文字列セグメント由来 (synthetic) か。
/// VisitorAdapter が confidence / usage role / macro callee 判定を行うため、
/// identifier のときだけ元ノードを保持する。
pub(crate) enum HitOrigin<'tree> {
    Identifier(Node<'tree>),
    Synthetic,
}

/// walk_refs が sink に渡す最小 hit 情報。context 抽出や列補正は sink 側で
/// 必要なときだけ行う (count 経路では一切行わない)。
pub(crate) struct RawRefHit<'idx, 'tree> {
    matches: MatchSet<'idx>,
    origin: HitOrigin<'tree>,
    line: usize,
    column: usize,
    is_def: bool,
}

/// walk 中に sink が共有参照する読み取り専用コンテキスト。`line_index` は context を
/// 必要とする sink (NEEDS_LINE_INDEX=true) のときだけ `Some`。count 経路では `None` で、
/// LineIndex 構築自体を省く。
pub(crate) struct RefEnvironment<'a> {
    source: &'a [u8],
    line_index: Option<&'a LineIndex>,
    lang_id: LangId,
    /// Rust の closure 束縛判定で使う名前別メモ。walk 1 回 (= 1 ファイル) の寿命に
    /// 閉じることで、ポインタ再利用による前ファイル結果の誤用を構造的に防ぐ。
    rust_binding_cache: RustPatternBindingCache,
}

impl RefEnvironment<'_> {
    /// 指定行の trim 済み context 文字列を返す (line_index を必要とする sink 専用)。
    #[inline]
    fn line_context(&self, row: usize) -> String {
        match self.line_index {
            Some(idx) => extract_line_context_indexed(self.source, idx, row),
            None => String::new(),
        }
    }

    /// context 行内の相対列を返す (インデント分を差し引く)。
    #[inline]
    fn ctx_column(&self, column: usize, row: usize) -> usize {
        match self.line_index {
            Some(idx) => context_column(column, self.source, idx, row),
            None => column,
        }
    }
}

/// identifier / segment を対象シンボルに照合する戦略。単一名検索 (テキスト比較) と
/// batch 検索 (name_to_ix lookup) の差をここに閉じ込める。
pub(crate) trait RefMatcher {
    fn identifier_matches(&self, node: Node<'_>, text: &str) -> Option<MatchSet<'_>>;
    fn segment_matches(&self, segment: &str) -> Option<MatchSet<'_>>;
}

/// 単一名検索用 matcher。`refs --name` の小規模検索で HashMap を作らずテキスト比較で
/// 照合する (1 要素 HashMap 化による退行を避ける)。一致時の index は常に 0。
pub(crate) struct SingleMatcher<'a> {
    pub(crate) lang_id: LangId,
    pub(crate) target: &'a str,
}

impl RefMatcher for SingleMatcher<'_> {
    #[inline]
    fn identifier_matches(&self, node: Node<'_>, text: &str) -> Option<MatchSet<'_>> {
        if ident_ref_matches(self.lang_id, node, text, self.target) {
            Some(MatchSet::One(0))
        } else {
            None
        }
    }

    #[inline]
    fn segment_matches(&self, segment: &str) -> Option<MatchSet<'_>> {
        if seg_ref_matches(self.lang_id, segment, self.target) {
            Some(MatchSet::One(0))
        } else {
            None
        }
    }
}

/// batch 検索用 matcher。正規化キーで name_to_ix を引き、一致した全 index を返す。
pub(crate) struct IndexedMatcher<'map, 'name> {
    pub(crate) lang_id: LangId,
    pub(crate) name_to_ix:
        &'map std::collections::HashMap<std::borrow::Cow<'name, str>, Vec<usize>>,
}

impl RefMatcher for IndexedMatcher<'_, '_> {
    #[inline]
    fn identifier_matches(&self, node: Node<'_>, text: &str) -> Option<MatchSet<'_>> {
        // `&str` で引くことで返り値スライスの寿命を name_to_ix (`'map`) だけに縛り、
        // 一時 Cow キー (text 借用) と切り離す。
        self.name_to_ix
            .get(&*node_ref_key(self.lang_id, node, text))
            .map(|ixs| MatchSet::Many(ixs.as_slice()))
    }

    #[inline]
    fn segment_matches(&self, segment: &str) -> Option<MatchSet<'_>> {
        self.name_to_ix
            .get(&*seg_ref_key(self.lang_id, segment))
            .map(|ixs| MatchSet::Many(ixs.as_slice()))
    }
}

/// walk_refs が hit ごとに呼ぶ出力先。context を必要とするか (NEEDS_LINE_INDEX) を
/// 型レベルで宣言し、不要な sink (CountSink) では呼び出し側が LineIndex 構築を省ける。
pub(crate) trait RawRefSink {
    const NEEDS_LINE_INDEX: bool;
    fn on_hit(&mut self, hit: RawRefHit<'_, '_>, env: &RefEnvironment<'_>);
}

/// hit を index 別の `Vec<SymbolReference>` に積む sink。単一名検索は長さ 1、
/// batch 検索は長さ num のバッファを渡すことで両者を兼ねる。
pub(crate) struct SymbolReferenceSink<'a> {
    pub(crate) buckets: &'a mut [Vec<SymbolReference>],
    pub(crate) path: &'a str,
}

impl RawRefSink for SymbolReferenceSink<'_> {
    const NEEDS_LINE_INDEX: bool = true;

    fn on_hit(&mut self, hit: RawRefHit<'_, '_>, env: &RefEnvironment<'_>) {
        let kind = Some(if hit.is_def {
            RefKind::Definition
        } else {
            RefKind::Reference
        });
        // context は 1 hit につき 1 回だけ抽出し、複数 index には clone で配る。
        let context = env.line_context(hit.line);
        match hit.matches {
            MatchSet::One(ix) => {
                self.buckets[ix].push(SymbolReference {
                    path: self.path.to_string(),
                    line: hit.line,
                    column: hit.column,
                    context: Some(context),
                    kind,
                    confidence: None,
                });
            }
            MatchSet::Many(ixs) => {
                for &ix in ixs {
                    self.buckets[ix].push(SymbolReference {
                        path: self.path.to_string(),
                        line: hit.line,
                        column: hit.column,
                        context: Some(context.clone()),
                        kind,
                        confidence: None,
                    });
                }
            }
        }
    }
}

/// hit を既存 `RefVisitEvent` に変換して `RefVisitor` へ流す sink。context_column /
/// confidence / macro callee / usage role の計算はこの adapter 内だけで行う。
pub(crate) struct VisitorAdapter<'v, V: RefVisitor> {
    pub(crate) visitor: &'v mut V,
}

impl<V: RefVisitor> RawRefSink for VisitorAdapter<'_, V> {
    const NEEDS_LINE_INDEX: bool = true;

    fn on_hit(&mut self, hit: RawRefHit<'_, '_>, env: &RefEnvironment<'_>) {
        let context = env.line_context(hit.line);
        let context_column = env.ctx_column(hit.column, hit.line);
        // identifier 由来のときだけ receiver-aware 分類 / macro callee / usage role を計算。
        // synthetic (文字列セグメント) は従来どおり ExactOwner / false / Other 固定。
        let (confidence, rust_macro_callee, usage) = match hit.origin {
            HitOrigin::Identifier(node) => (
                classify_method_ref_confidence(node, env.source, env.lang_id, hit.is_def),
                is_rust_macro_invocation_callee(node, env.lang_id),
                classify_ref_usage_role(node, env.lang_id),
            ),
            HitOrigin::Synthetic => (RefConfidence::ExactOwner, false, RefUsageRole::Other),
        };
        hit.matches.for_each_index(|ix| {
            self.visitor.on_ref(RefVisitEvent {
                sym_ix: ix as u32,
                line: hit.line,
                column: hit.column,
                context_column,
                context: &context,
                is_def: hit.is_def,
                confidence,
                rust_macro_callee,
                usage,
            });
        });
    }
}

/// 非 Definition 参照の件数のみ index 別に加算する sink。context を作らないため
/// NEEDS_LINE_INDEX=false で、呼び出し側の LineIndex 構築を省ける。
pub(crate) struct CountSink<'a> {
    pub(crate) counts: &'a mut [usize],
}

impl RawRefSink for CountSink<'_> {
    const NEEDS_LINE_INDEX: bool = false;

    fn on_hit(&mut self, hit: RawRefHit<'_, '_>, _env: &RefEnvironment<'_>) {
        // Definition は count 対象外 (旧 count_identifier_refs の guard 相当)。
        if hit.is_def {
            return;
        }
        hit.matches.for_each_index(|ix| self.counts[ix] += 1);
    }
}

// count 経路 (CountSink) は context を作らないため LineIndex 不要 = false を
// コンパイル時に固定する。`run_ref_walk` は NEEDS_LINE_INDEX=false のとき
// `LineIndex::new` を呼ばない (`bool::then` の意味論) ので、この不変条件により
// count 経路で LineIndex / context String が一切生成されないことが保証される。
// 対照的に context を返す SymbolReferenceSink は LineIndex を必要とする。
const _: () = assert!(!<CountSink<'static> as RawRefSink>::NEEDS_LINE_INDEX);
const _: () = assert!(<SymbolReferenceSink<'static> as RawRefSink>::NEEDS_LINE_INDEX);

/// synthetic 参照源 (文字列セグメント由来) 共通の hit 送出。segment が対象シンボルに
/// 一致すれば `HitOrigin::Synthetic` の hit を sink に流す。5 種の source ループは
/// walk_refs 内にそのまま残し、この 1 関数で送出処理だけを共有する。
#[inline]
fn emit_synthetic_hit<M: RefMatcher, S: RawRefSink>(
    segment: &str,
    row: usize,
    column: usize,
    matcher: &M,
    sink: &mut S,
    env: &RefEnvironment<'_>,
) {
    if let Some(matches) = matcher.segment_matches(segment) {
        sink.on_hit(
            RawRefHit {
                matches,
                origin: HitOrigin::Synthetic,
                line: row,
                column,
                is_def: false,
            },
            env,
        );
    }
}

/// 1 ノード分の参照判定。「identifier → 5 種 synthetic 源」の順に照合し、
/// 一致するたび `sink.on_hit` を呼ぶ。
fn visit_ref_node<M: RefMatcher, S: RawRefSink>(
    node: Node<'_>,
    matcher: &M,
    sink: &mut S,
    env: &RefEnvironment<'_>,
    definition_kinds: &[&str],
) {
    let source = env.source;
    let lang_id = env.lang_id;

    // (1) identifier ノード。ガード順 (matches → struct field 除外 → ignored 除外) は
    //     旧 collect 経路と同一。is_def は sink 側で用途が分かれるため常に算出する
    //     (count は Definition を弾き、collect/visitor は kind に反映)。
    if is_identifier_kind(node.kind())
        && let Ok(text) = node.utf8_text(source)
        && let Some(matches) = matcher.identifier_matches(node, text)
        && !(lang_id == LangId::Rust && is_rust_struct_field_non_callable(node))
        // closure パラメータが同名を束縛していれば、その配下の識別子は外側シンボルの
        // 参照ではない (シャドーイング)。参照として数えると dead-code が fail-open する。
        && !(lang_id == LangId::Rust
            && is_rust_closure_bound_identifier(node, text, source, &env.rust_binding_cache))
        && !is_ignored_identifier_context(node, lang_id)
    {
        let is_def = is_definition_context(node, definition_kinds, lang_id);
        let pos = node.start_position();
        sink.on_hit(
            RawRefHit {
                matches,
                origin: HitOrigin::Identifier(node),
                line: pos.row,
                column: pos.column,
                is_def,
            },
            env,
        );
    }

    // (2) Rust serde 属性文字列値
    for (seg, row, col) in rust_attr_string_ref_segments(node, source, lang_id) {
        emit_synthetic_hit(seg, row, col, matcher, sink, env);
    }

    // (3) bash `trap '<handler>' SIG` の handler 文字列
    for (seg, row, col) in bash_trap_handler_ref_segments(node, source, lang_id) {
        emit_synthetic_hit(&seg, row, col, matcher, sink, env);
    }

    // (4) PHPUnit DocBlock / attribute metadata
    for (seg, row, col) in phpunit_metadata_ref_segments(node, source, lang_id) {
        emit_synthetic_hit(&seg, row, col, matcher, sink, env);
    }

    // (5) PHP callable array `[Foo::class, 'method']`
    if let Some((method, row, col)) = php_callable_array_method_segment(node, source, lang_id) {
        emit_synthetic_hit(method, row, col, matcher, sink, env);
    }

    // (6) PHP 文字列 callable `'Class@method'`
    if let Some((method, row, col)) = php_string_callable_method_segment(node, source, lang_id) {
        emit_synthetic_hit(method, row, col, matcher, sink, env);
    }
}

/// 統合参照ウォーカー。単一 TreeCursor の反復 pre-order 走査で全ノードを
/// `visit_ref_node` に通す。訪問順 (親 → 子を宣言順) は旧再帰実装と同一。
///
/// 旧実装はノード毎に `node.walk()` で TreeCursor を C 側 malloc しており、
/// ノード数分の確保/解放と再帰の呼び出しオーバーヘッドが積み上がっていた。
/// depth ガードにより、subtree root で呼ばれても root の兄弟ノードへは進まない。
fn walk_refs<M: RefMatcher, S: RawRefSink>(
    root: Node<'_>,
    matcher: &M,
    sink: &mut S,
    env: &RefEnvironment<'_>,
    definition_kinds: &[&str],
) {
    let mut cursor = root.walk();
    let mut depth = 0usize;
    loop {
        visit_ref_node(cursor.node(), matcher, sink, env, definition_kinds);
        if cursor.goto_first_child() {
            depth += 1;
            continue;
        }
        loop {
            if depth == 0 {
                return;
            }
            if cursor.goto_next_sibling() {
                break;
            }
            cursor.goto_parent();
            depth -= 1;
        }
    }
}

/// walk_refs の呼び出しラッパー。sink が context を必要とする場合のみ LineIndex を
/// 構築する (count 経路では構築を完全に省く)。
pub(crate) fn run_ref_walk<M: RefMatcher, S: RawRefSink>(
    root: Node<'_>,
    source: &[u8],
    lang_id: LangId,
    definition_kinds: &[&str],
    matcher: &M,
    sink: &mut S,
) {
    let line_index = S::NEEDS_LINE_INDEX.then(|| LineIndex::new(source));
    let env = RefEnvironment {
        source,
        line_index: line_index.as_ref(),
        lang_id,
        rust_binding_cache: RustPatternBindingCache::default(),
    };
    walk_refs(root, matcher, sink, &env, definition_kinds);
}
