//! クロスファイル参照検索。
//!
//! 公開 API (`find_references` / `find_references_batch` /
//! `count_non_definition_refs_split_with_extra_files` 等) と、各ファイルを走査する
//! ドライバをここに置く。外部からの参照パス `crate::engine::refs::X` は、
//! 分割前と同じものが引けるよう本モジュールで再エクスポートする。

mod definition;
mod files;
mod lexer_path;
mod line_index;
mod role;
mod walker;

#[cfg(test)]
mod tests;

use anyhow::Result;
use rayon::prelude::*;
use std::path::Path;

use crate::engine::parser;
use crate::language::{LangId, normalize_identifier};
use crate::models::reference::{RefKind, SymbolReference};

pub use files::{collect_files, collect_files_with_excludes, merge_extra_files};
pub(crate) use line_index::{LineIndex, absolute_position, byte_offset_to_row_col};
pub(crate) use role::RefUsageRole;
pub(crate) use walker::{RefVisitEvent, RefVisitor};

use definition::definition_node_kinds;
use lexer_path::{count_refs_in_file_via_lexer, find_refs_batch_via_lexer, find_refs_via_lexer};
use walker::{
    CountSink, IndexedMatcher, SingleMatcher, SymbolReferenceSink, VisitorAdapter,
    build_name_to_ix, run_ref_walk,
};

/// `find_references` / `find_references_batch` 用の最大並列ワーカー数。
///
/// 数万ファイル級の大規模リポジトリでは rayon fold バケットがワーカー数に比例して
/// `Vec<SymbolReference>` を抱えるため、物理コア数をそのまま使うと RSS が線形に膨張し
/// OOM を招く。`ASTRO_SIGHT_BATCH_WORKERS` で上書き可能。
fn bounded_worker_count() -> usize {
    std::env::var("ASTRO_SIGHT_BATCH_WORKERS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(4)
}

/// `find_references` / `find_references_batch` 共通の上限付き rayon プールを構築する。
///
/// ワーカー毎に fold バケット (`Vec<SymbolReference>` / `Vec<Vec<SymbolReference>>`) を
/// 抱えるため、物理コア数をそのまま使うと大規模リポで RSS が線形に膨張し OOM を招く。
/// 物理コア数と `bounded_worker_count()` の小さい方でワーカー数を制限する。
fn build_bounded_pool() -> Result<rayon::ThreadPool> {
    let worker_limit = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(bounded_worker_count());
    rayon::ThreadPoolBuilder::new()
        .num_threads(worker_limit)
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to build rayon pool: {e}"))
}

/// `find_references_batch` の内部 chunk サイズ。既定 64。
/// AC trie はパターン数に対して非線形にメモリを使い、fold バケットも名前数分
/// 確保されるため、名前を chunk 分割して trie / バケットを chunk サイズで上限する。
/// `ASTRO_SIGHT_REFS_BATCH_CHUNK` で上書き可能。
fn refs_batch_chunk_size() -> usize {
    std::env::var("ASTRO_SIGHT_REFS_BATCH_CHUNK")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(64)
}

/// AC マルチパターン事前フィルタ。`source` を走査して出現した name index の集合を返す。
/// `num_names` 個すべて出現した時点で走査を打ち切る (超集合フィルタ、AC は ASCII CI 構築前提)。
/// 空集合の場合は呼び出し側が各々の早期 return (空結果) を行う。
fn ac_present_indices(
    ac: &aho_corasick::AhoCorasick,
    source: &[u8],
    num_names: usize,
) -> std::collections::HashSet<usize> {
    let mut present_indices: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for mat in ac.find_overlapping_iter(source) {
        present_indices.insert(mat.pattern().as_usize());
        if present_indices.len() == num_names {
            break;
        }
    }
    present_indices
}

/// 指定シンボルへの参照をディレクトリ内のファイルから検索する。
/// glob パターン（例: "**/*.rs"）によるフィルタも可能。
pub fn find_references(
    symbol_name: &str,
    dir: &Path,
    glob_pattern: Option<&str>,
) -> Result<Vec<SymbolReference>> {
    let files = collect_files(dir, glob_pattern)?;

    let pool = build_bounded_pool()?;

    // per-file Vec を全ファイル分保持せず、worker local の Vec へ直接統合する。
    let mut all_refs: Vec<SymbolReference> = pool.install(|| {
        files
            .into_par_iter()
            .fold(Vec::new, |mut local, path| {
                if let Some(path_str) = path.to_str() {
                    let utf8_path = camino::Utf8Path::new(path_str);
                    if let Ok(mut refs) = find_refs_in_file(symbol_name, utf8_path) {
                        local.append(&mut refs);
                    }
                }
                local
            })
            .reduce(Vec::new, |mut acc, mut local| {
                acc.append(&mut local);
                acc
            })
    });

    // Angular template (`*.component.html` / inline `template:`) のバインディング式から
    // の参照を追加する。TS の AST 参照だけでは外部テンプレート経由の呼び出しを取りこぼす
    // ため (GitLab #18)。非 Angular プロジェクトでは空を返し副作用なし。
    all_refs.extend(
        crate::engine::angular_template_refs::find_angular_template_references(
            symbol_name,
            dir,
            glob_pattern,
        ),
    );

    sort_references(&mut all_refs);

    Ok(all_refs)
}

fn sort_references(refs: &mut [SymbolReference]) {
    // ソート: 定義を先頭に、その後パス/行番号順
    refs.sort_by(|a, b| {
        let def_order = |k: &Option<RefKind>| match k {
            Some(RefKind::Definition) => 0,
            _ => 1,
        };
        def_order(&a.kind)
            .cmp(&def_order(&b.kind))
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.line.cmp(&b.line))
    });
}

/// 単一ファイル内でシンボル参照を検索する。
///
/// lexer-only 言語 (現状 Xojo) は手書き lexer で identifier 列挙する。
/// tree-sitter 系は従来通り Query + AST 走査で確定検証する。
fn find_refs_in_file(symbol_name: &str, path: &camino::Utf8Path) -> Result<Vec<SymbolReference>> {
    let source = parser::read_file(path)?;

    // ファイル言語を拡張子から先読みし、CI 言語ではバイト事前フィルタを skip
    // (memchr は case-sensitive のため Xojo の `MyVar`/`myvar` 一致を取りこぼす)。
    let ext_lang = LangId::from_path(path).ok();
    let is_ci = ext_lang.is_some_and(|l| l.is_case_insensitive());
    if !is_ci {
        // PHP は関数/メソッド/クラス名が case-insensitive なため、大小無視で事前フィルタして
        // case 違いの参照を取りこぼさない。他の case-sensitive 言語は従来どおり memmem で弾く。
        let present = if ext_lang == Some(LangId::Php) {
            let needle = symbol_name.to_ascii_lowercase();
            memchr::memmem::find(&source.to_ascii_lowercase(), needle.as_bytes()).is_some()
        } else {
            memchr::memmem::find(&source, symbol_name.as_bytes()).is_some()
        };
        if !present {
            return Ok(Vec::new());
        }
    }

    // lexer-only 言語は parse_file を呼ばず lexer 経由で identifier 列挙する。
    if let Some(lang) = ext_lang
        && let crate::language::DetectedLang::LexerOnly(lexer_lang) = lang.detected()
    {
        return Ok(find_refs_via_lexer(symbol_name, &source, path, lexer_lang));
    }

    let (tree, lang_id) = parser::parse_file(path, &source)?;
    let root = tree.root_node();

    let definition_kinds = definition_node_kinds(lang_id);
    let target = normalize_identifier(lang_id, symbol_name);
    let matcher = SingleMatcher {
        lang_id,
        target: target.as_ref(),
    };
    // 単一名検索は長さ 1 のバッファへ集約し、SymbolReferenceSink を batch と共用する。
    let mut buckets = vec![Vec::new()];
    let mut sink = SymbolReferenceSink {
        buckets: &mut buckets,
        path: path.as_str(),
    };
    run_ref_walk(
        root,
        &source,
        lang_id,
        definition_kinds,
        &matcher,
        &mut sink,
    );

    Ok(buckets.into_iter().next().unwrap_or_default())
}

// ---------------------------------------------------------------------------
// バッチ参照検索: O(S × N) ではなく O(N + S) で処理する
// ---------------------------------------------------------------------------

/// 全シンボル名の参照を1回のディレクトリウォークで検索する。
/// シンボル名→参照リストのマップを返す。
/// Aho-Corasick オートマトンによる効率的なマルチパターン事前フィルタを使用。
///
/// fold/reduce でワーカー局所バケットに直接統合し、
/// per_file Vec + merged HashMap の二重保持を回避する。
pub fn find_references_batch(
    symbol_names: &[String],
    dir: &Path,
    glob_pattern: Option<&str>,
) -> Result<std::collections::HashMap<String, Vec<SymbolReference>>> {
    use std::collections::HashMap;

    if symbol_names.is_empty() {
        return Ok(HashMap::new());
    }

    // ディレクトリウォーク + 全ファイルの生成物マーカー読込 (先頭 4KB) は名前数に依らず
    // 1 回で済ます。以前は呼び出し側が名前を chunk 分割して本関数を chunk 回呼んでいたため
    // walk が ceil(N / chunk) 回繰り返され、大規模リポでは参照検索の純コストが walk の
    // 再実行に支配されていた。files / pool を全 chunk で共有して 1 回に集約する
    // (count_non_definition_refs_split と同じ手法)。
    let files = collect_files(dir, glob_pattern)?;

    // rayon のワーカー数を上限付きにする (バケットのピーク RSS 抑制、詳細は build_bounded_pool)。
    let pool = build_bounded_pool()?;

    // AC trie はパターン数に対して非線形にメモリを使い、fold バケットも名前数分確保される。
    // 名前を chunk 分割して chunk 毎に trie 構築 → 走査 → drop し、ピーク RSS を chunk
    // サイズに対して定数で抑える。files / pool は全 chunk で共有する。
    let mut merged: HashMap<String, Vec<SymbolReference>> =
        HashMap::with_capacity(symbol_names.len());

    // Angular template scan の前処理 (canonicalize / `is_angular_project` の全 dir 走査 /
    // `collect_component_templates` の全 `.ts` 走査) は chunk 数に依らず 1 回で済ます。
    // 非 Angular リポでは `None` となり、chunk ループ内で template scan を完全に skip する。
    let angular_ctx =
        crate::engine::angular_template_refs::AngularBatchContext::prepare(dir, glob_pattern);

    for chunk in symbol_names.chunks(refs_batch_chunk_size()) {
        // AC は ASCII CI で構築: CI 言語 (Xojo) で case 違いを事前フィルタで取りこぼさない
        // ため。非 CI 言語では多少の false positive (大文字小文字違い) が発生するが、AST
        // 比較で弾く。
        let ac = aho_corasick::AhoCorasick::builder()
            .ascii_case_insensitive(true)
            .build(chunk)
            .map_err(|e| anyhow::anyhow!("Failed to build pattern matcher: {e}"))?;

        // fold/reduce: ワーカーごとに Vec<Vec<SymbolReference>> を持ち、直接統合する。
        // files は借用で共有し、chunk 毎に walk し直さない。
        let mut buckets: Vec<Vec<SymbolReference>> = pool.install(|| {
            files
                .par_iter()
                .fold(
                    || vec![Vec::new(); chunk.len()],
                    |mut local, path| {
                        let Some(path_str) = path.to_str() else {
                            return local;
                        };
                        let utf8_path = camino::Utf8Path::new(path_str);
                        if let Ok(per_file) = find_refs_batch_in_file_indexed(chunk, &ac, utf8_path)
                        {
                            for (ix, mut refs) in per_file.into_iter().enumerate() {
                                local[ix].append(&mut refs);
                            }
                        }
                        local
                    },
                )
                .reduce(
                    || vec![Vec::new(); chunk.len()],
                    |mut acc, mut local| {
                        for (acc_refs, local_refs) in acc.iter_mut().zip(local.iter_mut()) {
                            acc_refs.append(local_refs);
                        }
                        acc
                    },
                )
        });

        // Angular template バインディング式からの参照を chunk 分まとめて統合する (GitLab #18)。
        // テンプレートを名前数分スキャンせず chunk 単位で全名を 1 回で引く。
        // 事前に組み立てた AngularBatchContext を使い、chunk 数倍の全 dir/.ts 走査を避ける。
        if let Some(ctx) = angular_ctx.as_ref() {
            let template_refs =
                crate::engine::angular_template_refs::find_angular_template_references_batch_with_context(
                    chunk, ctx,
                );
            for (bucket, mut t) in buckets.iter_mut().zip(template_refs) {
                bucket.append(&mut t);
            }
        }

        for (i, name) in chunk.iter().enumerate() {
            let mut refs = std::mem::take(&mut buckets[i]);
            sort_references(&mut refs);
            if !refs.is_empty() {
                merged.insert(name.clone(), refs);
            }
        }
    }

    Ok(merged)
}

/// impact analyze 用: symbol_names を AC 事前フィルタで 1 回構築して返す。
/// streaming Pass から per-file 呼び出しのためのユーティリティ。
pub(crate) fn build_ac_case_insensitive(
    symbol_names: &[String],
) -> Result<aho_corasick::AhoCorasick> {
    aho_corasick::AhoCorasick::builder()
        .ascii_case_insensitive(true)
        .build(symbol_names)
        .map_err(|e| anyhow::anyhow!("Failed to build pattern matcher: {e}"))
}

/// dead-code 判定用 (追加ファイル対応版)。通常の workspace walk で得たファイルに、
/// diff 由来などの明示ファイルを canonical path で追加して参照件数を集計する。
/// hidden ディレクトリ配下でも候補になったファイル自身の参照を取りこぼさないために使う。
pub fn count_non_definition_refs_split_with_extra_files<F>(
    symbol_names: &[String],
    dir: &Path,
    glob_pattern: Option<&str>,
    extra_files: &[std::path::PathBuf],
    is_test: F,
) -> Result<std::collections::HashMap<String, (usize, usize)>>
where
    F: Fn(&Path) -> bool + Sync,
{
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    if symbol_names.is_empty() {
        return Ok(HashMap::new());
    }

    let canonical_dir = std::fs::canonicalize(dir)?;
    let mut files = collect_files(&canonical_dir, glob_pattern)?;
    merge_extra_files(&mut files, &canonical_dir, extra_files);

    let n = symbol_names.len();
    // shared atomic counters: rayon の chunk 単位で `(vec![0; n], vec![0; n])` を都度確保せず、
    // n × 16 bytes 一定のメモリで全 worker から fetch_add する。
    // dead-code は unique_names が大規模リポジトリで数万〜数十万件に達するため、
    // fold の per-chunk Vec が同時確保されると数 GB の bursty allocation を招いていた。
    let prod_counts: Vec<AtomicUsize> = (0..n).map(|_| AtomicUsize::new(0)).collect();
    let test_counts: Vec<AtomicUsize> = (0..n).map(|_| AtomicUsize::new(0)).collect();

    // AC trie はパターン数に対して非線形にメモリを食う。dead-code 経路で
    // unique_names が数万件まで膨らむと trie 自体が GB 級になり、起動直後の
    // 一括確保で OOM の主因になっていた。chunk 単位で構築 → 走査 → drop して
    // ピーク RSS を AC_CHUNK_SIZE に対して定数で抑える。
    const AC_CHUNK_SIZE: usize = 1024;
    for (chunk_offset, chunk_start) in (0..n).step_by(AC_CHUNK_SIZE).enumerate() {
        let chunk_end = (chunk_start + AC_CHUNK_SIZE).min(n);
        let chunk = &symbol_names[chunk_start..chunk_end];
        let base_idx = chunk_offset * AC_CHUNK_SIZE;

        let ac = aho_corasick::AhoCorasick::builder()
            .ascii_case_insensitive(true)
            .build(chunk)
            .map_err(|e| anyhow::anyhow!("Failed to build pattern matcher: {e}"))?;

        files.par_iter().for_each(|path| {
            let Some(path_str) = path.to_str() else {
                return;
            };
            let utf8_path = camino::Utf8Path::new(path_str);
            if let Ok(per_file) = count_refs_in_file(chunk, &ac, utf8_path) {
                let bucket = if is_test(path) {
                    &test_counts
                } else {
                    &prod_counts
                };
                for (local_ix, cnt) in per_file.into_iter().enumerate() {
                    if cnt != 0 {
                        bucket[base_idx + local_ix].fetch_add(cnt, Ordering::Relaxed);
                    }
                }
            }
        });
        // ac はここで drop され、次 chunk まで AC trie のメモリは解放される
    }

    let mut out = HashMap::with_capacity(n);
    for (i, name) in symbol_names.iter().enumerate() {
        out.insert(
            name.clone(),
            (
                prod_counts[i].load(Ordering::Relaxed),
                test_counts[i].load(Ordering::Relaxed),
            ),
        );
    }
    Ok(out)
}

/// visitor callback 版の per-file ref 走査。
///
/// `SymbolReference` を 1 件も生成せず、identifier にヒットした瞬間に `visitor.on_ref`
/// を直接呼ぶため、per-file の `Vec<Vec<SymbolReference>>` に起因する heap 確保を完全に
/// 廃止できる。呼び出し側（impact streaming Pass）で filter + intern まで一気に処理する。
pub(crate) fn visit_refs_and_defs_in_file_cb<V: RefVisitor>(
    symbol_names: &[String],
    ac: &aho_corasick::AhoCorasick,
    path: &camino::Utf8Path,
    visitor: &mut V,
) -> Result<()> {
    let num = symbol_names.len();
    let source = parser::read_file(path)?;

    let present_indices = ac_present_indices(ac, source.as_bytes(), num);
    if present_indices.is_empty() {
        return Ok(());
    }

    let (tree, lang_id) = parser::parse_file(path, &source)?;
    let root = tree.root_node();
    let definition_kinds = definition_node_kinds(lang_id);

    let name_to_ix = build_name_to_ix(lang_id, symbol_names, &present_indices);

    let matcher = IndexedMatcher {
        lang_id,
        name_to_ix: &name_to_ix,
    };
    let mut sink = VisitorAdapter { visitor };
    run_ref_walk(
        root,
        &source,
        lang_id,
        definition_kinds,
        &matcher,
        &mut sink,
    );
    Ok(())
}

/// 単一ファイル内で複数シンボルの参照を index ベースの Vec に格納する。
/// find_references_batch の fold/reduce および impact analyze の streaming Pass から呼ばれる。
pub(crate) fn find_refs_batch_in_file_indexed(
    symbol_names: &[String],
    ac: &aho_corasick::AhoCorasick,
    path: &camino::Utf8Path,
) -> Result<Vec<Vec<SymbolReference>>> {
    let num = symbol_names.len();
    let source = parser::read_file(path)?;

    // マルチパターン事前フィルタ (AC は ASCII CI で構築済、超集合フィルタ)
    let present_indices = ac_present_indices(ac, source.as_bytes(), num);
    if present_indices.is_empty() {
        return Ok(vec![Vec::new(); num]);
    }

    // lexer-only 言語は parse_file を呼ばず lexer 経路で identifier 列挙する。
    if let Ok(lang) = LangId::from_path(path)
        && let crate::language::DetectedLang::LexerOnly(lexer_lang) = lang.detected()
    {
        return Ok(find_refs_batch_via_lexer(
            symbol_names,
            &present_indices,
            &source,
            path,
            lexer_lang,
        ));
    }

    let (tree, lang_id) = parser::parse_file(path, &source)?;
    let root = tree.root_node();
    let definition_kinds = definition_node_kinds(lang_id);

    // 言語別に正規化キーで name_to_ix を構築する (Xojo は case 折りたたみ、PHP は
    // 関数/メソッド/クラス系の case-insensitive 参照に備え folded キーも登録する)。
    let name_to_ix = build_name_to_ix(lang_id, symbol_names, &present_indices);

    let mut result = vec![Vec::new(); num];
    let matcher = IndexedMatcher {
        lang_id,
        name_to_ix: &name_to_ix,
    };
    let mut sink = SymbolReferenceSink {
        buckets: &mut result,
        path: path.as_str(),
    };
    run_ref_walk(
        root,
        &source,
        lang_id,
        definition_kinds,
        &matcher,
        &mut sink,
    );

    Ok(result)
}

/// 単一ファイル内の非 Definition 参照件数をカウントする（SymbolReference を確保しない）。
fn count_refs_in_file(
    symbol_names: &[String],
    ac: &aho_corasick::AhoCorasick,
    path: &camino::Utf8Path,
) -> Result<Vec<usize>> {
    let num = symbol_names.len();
    let source = parser::read_file(path)?;

    let present_indices = ac_present_indices(ac, source.as_bytes(), num);
    if present_indices.is_empty() {
        return Ok(vec![0; num]);
    }

    // lexer-only 言語 (現状 Xojo) は tree-sitter parse を持たないため、lexer 経路で
    // 非定義参照のみカウントする。dispatch が漏れると `parse_file` が
    // `UNSUPPORTED_LANGUAGE` を返して `?` でエラーになり、AC で hit していた count が
    // 0 になって dead-code 誤検出の温床になる (GitLab #9 で報告された Xojo の大量誤検出
    // の根本原因)。
    if let Ok(lang) = LangId::from_path(path)
        && let crate::language::DetectedLang::LexerOnly(lexer_lang) = lang.detected()
    {
        return Ok(count_refs_in_file_via_lexer(
            symbol_names,
            &present_indices,
            &source,
            lexer_lang,
        ));
    }

    let (tree, lang_id) = parser::parse_file(path, &source)?;
    let root = tree.root_node();
    let definition_kinds = definition_node_kinds(lang_id);

    // 言語別に正規化キーで name_to_ix を構築 (Xojo は case 折りたたみ、PHP は関数/メソッド/
    // クラス系の case-insensitive 参照に備え folded キーも登録)。
    let name_to_ix = build_name_to_ix(lang_id, symbol_names, &present_indices);

    let mut counts = vec![0usize; num];
    let matcher = IndexedMatcher {
        lang_id,
        name_to_ix: &name_to_ix,
    };
    let mut sink = CountSink {
        counts: &mut counts,
    };
    // CountSink::NEEDS_LINE_INDEX = false のため run_ref_walk は LineIndex を構築せず、
    // context 文字列も一切生成しない (軽量 count 経路)。
    run_ref_walk(
        root,
        &source,
        lang_id,
        definition_kinds,
        &matcher,
        &mut sink,
    );

    Ok(counts)
}
