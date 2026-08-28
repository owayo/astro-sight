//! `refs` の単体テスト。テーマ別サブモジュールに分ける。

mod definition;
mod files;
mod line_context;
mod php;
mod rust;
mod walker;

use tree_sitter::Node;

use super::*;

// 本体側の各サブモジュールに散った被テスト項目を、テストモジュール直下へ一括で
// 引き込む。サブモジュール側は `use super::*;` だけでこれらを引けるようにし、
// テスト本体は分割前と同じ名前 (`super::is_generated_file` 等) のまま動かす。
use super::definition::is_identifier_kind;
use super::definition::php::{
    php_callable_array_method_segment, php_string_callable_method_segment,
};
use super::definition::rust::{rust_attr_string_ref_segments, split_path_segments};
use super::files::is_generated_file;
use super::line_index::{extract_line_context_bytes_indexed, extract_line_context_indexed};
use super::role::classify_ref_usage_role;

/// テスト用: 単一名の in-memory 参照収集 (SingleMatcher + SymbolReferenceSink)。
/// 旧 `collect_identifier_refs` を直接叩いていた単体テストの置き換え。
fn collect_single_refs_for_test(
    root: Node<'_>,
    source: &[u8],
    target: &str,
    path: &str,
    definition_kinds: &[&str],
    lang_id: LangId,
) -> Vec<SymbolReference> {
    let matcher = SingleMatcher { lang_id, target };
    let mut buckets = vec![Vec::new()];
    let mut sink = SymbolReferenceSink {
        buckets: &mut buckets,
        path,
    };
    run_ref_walk(root, source, lang_id, definition_kinds, &matcher, &mut sink);
    buckets.into_iter().next().unwrap_or_default()
}

/// テスト用: index ベースの参照収集 (IndexedMatcher + SymbolReferenceSink)。
/// single 経路 (`collect_single_refs_for_test`) との突き合わせに使う。
fn collect_batch_refs_for_test(
    root: Node<'_>,
    source: &[u8],
    symbol_names: &[String],
    path: &str,
    definition_kinds: &[&str],
    lang_id: LangId,
) -> Vec<Vec<SymbolReference>> {
    let present: std::collections::HashSet<usize> = (0..symbol_names.len()).collect();
    let name_index = super::walker::build_name_index(lang_id, symbol_names, &present);
    let matcher = IndexedMatcher {
        name_index: &name_index,
    };
    let mut buckets = vec![Vec::new(); symbol_names.len()];
    let mut sink = SymbolReferenceSink {
        buckets: &mut buckets,
        path,
    };
    run_ref_walk(root, source, lang_id, definition_kinds, &matcher, &mut sink);
    buckets
}

/// テスト用: 参照リストを (line, column, kind) の順序付き集合へ畳む。
/// single / batch の突き合わせで件数だけでなく位置と分類まで比較するために使う。
fn ref_fingerprints(refs: &[SymbolReference]) -> Vec<(usize, usize, String)> {
    let mut v: Vec<_> = refs
        .iter()
        .map(|r| (r.line, r.column, format!("{:?}", r.kind)))
        .collect();
    v.sort();
    v
}

/// テスト用: index ベースの非 Definition 参照カウント (IndexedMatcher + CountSink)。
/// 旧 `count_identifier_refs` を直接叩いていた単体テストの置き換え。
///
/// `symbol_names` から `build_name_index` で実 index を組む (map を手で作ると照合ドメインの
/// 分離を迂回してしまい、batch 経路の回帰を検出できない)。
fn count_refs_for_test(
    root: Node<'_>,
    source: &[u8],
    symbol_names: &[String],
    definition_kinds: &[&str],
    lang_id: LangId,
    num: usize,
) -> Vec<usize> {
    let present: std::collections::HashSet<usize> = (0..symbol_names.len()).collect();
    let name_index = super::walker::build_name_index(lang_id, symbol_names, &present);
    let matcher = IndexedMatcher {
        name_index: &name_index,
    };
    let mut counts = vec![0usize; num];
    let mut sink = CountSink {
        counts: &mut counts,
    };
    run_ref_walk(root, source, lang_id, definition_kinds, &matcher, &mut sink);
    counts
}
