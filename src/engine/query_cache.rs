//! tree-sitter Query のプロセス内キャッシュ。
//!
//! `Query::new` は文法サイズに比例するパターン解析 (`ts_query__perform_analysis`) を
//! 伴い、TypeScript 級の文法ではファイルの parse 本体より数倍重い (実測: `symbols --dir`
//! のプロファイルでクエリコンパイルがパースの約 7 倍)。クエリ文字列は言語毎に固定の
//! built-in が大半のため、(LangId, クエリ文字列) 単位でコンパイル結果をプロセス内で
//! 共有する。tree-sitter 0.26 の `Query` は Send + Sync (ビルド後 immutable) なので
//! Arc で全スレッド共有できる。`QueryCursor` は状態を持つため共有せず従来どおり
//! 呼び出し毎に作る。

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, RwLock};

use tree_sitter::Query;

use crate::language::LangId;

/// キャッシュ総数の上限。built-in クエリは言語数 × クエリ種別で数十件に収まる。
/// custom query (`symbols --query` / lint ルール) が MCP / session の常駐プロセスへ
/// 際限なく流入してもメモリが増え続けないよう、上限到達後の新規クエリはキャッシュせず
/// 都度コンパイルへフォールバックする (結果は不変、速度だけ従来相当に戻る)。
const MAX_CACHED_QUERIES: usize = 512;

/// 言語毎の「クエリ文字列 → コンパイル済み Query」マップ。
type PerLangQueries = HashMap<String, Arc<Query>>;

static CACHE: LazyLock<RwLock<HashMap<LangId, PerLangQueries>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// (lang_id, query_src) のコンパイル済み Query を返す。未キャッシュならコンパイルして
/// 登録する。呼び出し前提は従来の `Query::new(&lang_id.ts_language(), ..)` と同じで、
/// lexer-only 言語を渡してはならない (`ts_language()` が panic する)。
///
/// コンパイルは lock の外で行う (ms 級の処理で read/write lock を塞がないため)。
/// 同一クエリを複数 worker が同時にコンパイルした場合も結果は同一で、`or_insert_with`
/// により先着エントリが残るだけなので正しさに影響しない。
pub fn cached_query(
    lang_id: LangId,
    query_src: &str,
) -> Result<Arc<Query>, tree_sitter::QueryError> {
    if let Some(query) = CACHE
        .read()
        .expect("query cache poisoned")
        .get(&lang_id)
        .and_then(|per_lang| per_lang.get(query_src))
    {
        return Ok(Arc::clone(query));
    }

    let query = Arc::new(Query::new(&lang_id.ts_language(), query_src)?);

    let mut cache = CACHE.write().expect("query cache poisoned");
    let total: usize = cache.values().map(HashMap::len).sum();
    if total < MAX_CACHED_QUERIES {
        cache
            .entry(lang_id)
            .or_default()
            .entry(query_src.to_string())
            .or_insert_with(|| Arc::clone(&query));
    }
    Ok(query)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 同一 (lang, query) は同一 Arc を返し、再コンパイルしない。
    #[test]
    fn cached_query_returns_shared_instance() {
        let src = "(function_item name: (identifier) @function.name)";
        let a = cached_query(LangId::Rust, src).expect("compile");
        let b = cached_query(LangId::Rust, src).expect("compile");
        assert!(Arc::ptr_eq(&a, &b));
    }

    /// 言語が違えば別エントリになる (同一クエリ文字列でも混線しない)。
    #[test]
    fn cached_query_is_keyed_by_language() {
        let src = "(identifier) @c";
        let rust = cached_query(LangId::Rust, src).expect("compile rust");
        let python = cached_query(LangId::Python, src).expect("compile python");
        assert!(!Arc::ptr_eq(&rust, &python));
    }

    /// 不正クエリはキャッシュされずエラーを返す。
    #[test]
    fn cached_query_propagates_compile_errors() {
        assert!(cached_query(LangId::Rust, "(nonexistent_node_kind) @x").is_err());
    }
}
