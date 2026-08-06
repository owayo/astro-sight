//! lexer-only 言語 (現状 Xojo) 向けの参照検索経路。
//!
//! tree-sitter parse を持たない言語のため、手書き lexer の identifier 走査結果を
//! 定義ヘッダ行と突き合わせて Definition / Reference に分類する。

use crate::models::reference::{RefKind, SymbolReference};

use super::line_index::{LineIndex, extract_line_context_bytes_indexed};

/// lexer-only ファイル向けの参照検索。
///
/// identifier トークン位置を列挙し、定義ヘッダ行 (Class/Sub/Function 等) と
/// 一致するものを Definition、それ以外を Reference として返す。
pub(crate) fn find_refs_via_lexer(
    symbol_name: &str,
    source: &[u8],
    path: &camino::Utf8Path,
    lexer_lang: crate::language::LexerLang,
) -> Vec<SymbolReference> {
    use crate::engine::lexer;

    // profile はシンボル毎に不変なのでループ外で 1 度だけ引く (batch 版と同形)。
    let profile = lexer::profile_for(lexer_lang);
    // 定義ヘッダ位置 (行番号) のセット。lexer profile 経由で抽出。
    let def_lines: std::collections::HashSet<usize> = lexer::extract_symbols(source, lexer_lang)
        .iter()
        .filter(|s| {
            // 大小無視で名前一致するもののみ抽出 (Xojo は case-insensitive)。
            if profile.case_insensitive {
                s.name.eq_ignore_ascii_case(symbol_name)
            } else {
                s.name == symbol_name
            }
        })
        .map(|s| s.range.start.line)
        .collect();

    let names = vec![symbol_name.to_string()];
    let bucket = lexer::find_identifier_refs(source, &names, lexer_lang);
    let matches = bucket
        .into_iter()
        .next()
        .map(|(_, v)| v)
        .unwrap_or_default();

    // 同一 source 内で M 件の line context を取り出すため、改行 index を 1 度だけ構築する。
    let line_index = LineIndex::new(source);
    // lexer 経路は 0-indexed line で統一済み (tree-sitter::Point と同じ)。
    matches
        .into_iter()
        .map(|m| {
            let is_def = def_lines.contains(&m.line);
            SymbolReference {
                path: path.as_str().to_string(),
                line: m.line,
                column: m.column,
                context: Some(extract_line_context_bytes_indexed(
                    source,
                    &line_index,
                    m.line,
                )),
                kind: Some(if is_def {
                    RefKind::Definition
                } else {
                    RefKind::Reference
                }),
                confidence: None,
            }
        })
        .collect()
}

/// dead-code 用の count-only lexer fallback。`find_refs_batch_via_lexer` と異なり
/// `SymbolReference` を作らず `Vec<usize>` だけ返すため、巨大リポでの per-symbol Vec
/// 確保を避けてピーク RSS を抑える。
pub(crate) fn count_refs_in_file_via_lexer(
    symbol_names: &[String],
    present_indices: &std::collections::HashSet<usize>,
    source: &[u8],
    lexer_lang: crate::language::LexerLang,
) -> Vec<usize> {
    let num = symbol_names.len();
    if present_indices.is_empty() {
        return vec![0; num];
    }
    // AC で hit した names だけを lexer 走査対象に絞る。
    let active_indices: Vec<usize> = present_indices.iter().copied().collect();
    let active_names: Vec<String> = active_indices
        .iter()
        .map(|&i| symbol_names[i].clone())
        .collect();

    let partial =
        crate::engine::lexer::count_non_definition_refs(source, &active_names, lexer_lang);

    let mut counts = vec![0usize; num];
    for (i, &orig_ix) in active_indices.iter().enumerate() {
        counts[orig_ix] = partial[i];
    }
    counts
}

/// batch 経路向け lexer fallback。複数 symbol の参照を一度の走査で集める。
pub(crate) fn find_refs_batch_via_lexer(
    symbol_names: &[String],
    present_indices: &std::collections::HashSet<usize>,
    source: &[u8],
    path: &camino::Utf8Path,
    lexer_lang: crate::language::LexerLang,
) -> Vec<Vec<SymbolReference>> {
    use crate::engine::lexer;

    let num = symbol_names.len();
    let mut result: Vec<Vec<SymbolReference>> = vec![Vec::new(); num];

    // AC で存在を確認できた names のみ走査する (CI 言語でも safe: AC は ASCII CI 構築済)。
    let active_indices: Vec<usize> = present_indices.iter().copied().collect();
    if active_indices.is_empty() {
        return result;
    }

    let active_names: Vec<String> = active_indices
        .iter()
        .map(|&i| symbol_names[i].clone())
        .collect();

    // 定義ヘッダ行を 1 回だけ抽出してキャッシュする (case_insensitive 正規化キー → 行集合)。
    let profile = lexer::profile_for(lexer_lang);
    let normalize = |s: &str| -> String {
        if profile.case_insensitive {
            s.to_ascii_lowercase()
        } else {
            s.to_string()
        }
    };
    let mut def_lines: std::collections::HashMap<String, std::collections::HashSet<usize>> =
        std::collections::HashMap::new();
    for sym in lexer::extract_symbols(source, lexer_lang) {
        def_lines
            .entry(normalize(&sym.name))
            .or_default()
            .insert(sym.range.start.line);
    }

    // 同一 source 内で M 件の line context を取り出すため、改行 index を 1 度だけ構築する。
    let line_index = LineIndex::new(source);
    let bucket = lexer::find_identifier_refs(source, &active_names, lexer_lang);
    for (i, (name, matches)) in active_indices.iter().zip(bucket).enumerate() {
        let normalized = normalize(&active_names[i]);
        let def_set = def_lines.get(&normalized).cloned().unwrap_or_default();
        let path_str = path.as_str().to_string();
        for m in matches.1 {
            let is_def = def_set.contains(&m.line);
            result[*name].push(SymbolReference {
                path: path_str.clone(),
                line: m.line,
                column: m.column,
                context: Some(extract_line_context_bytes_indexed(
                    source,
                    &line_index,
                    m.line,
                )),
                kind: Some(if is_def {
                    RefKind::Definition
                } else {
                    RefKind::Reference
                }),
                confidence: None,
            });
        }
    }

    result
}
