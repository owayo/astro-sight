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
use crate::models::skip::SkippedFiles;

pub use files::{
    FileCollection, FileScanOptions, collect_files, collect_files_scan,
    collect_files_with_excludes, merge_extra_files,
};
pub(crate) use line_index::{LineIndex, absolute_position, byte_offset_to_row_col};
pub(crate) use role::RefUsageRole;
pub(crate) use walker::{RefVisitEvent, RefVisitor};

use definition::definition_node_kinds;
use lexer_path::{count_refs_in_file_via_lexer, find_refs_batch_via_lexer, find_refs_via_lexer};
use walker::{
    CountSink, IndexedMatcher, SingleMatcher, SymbolReferenceSink, VisitorAdapter,
    build_name_index, run_ref_walk,
};

/// `find_references` / `find_references_batch` 用の最大並列ワーカー数。
///
/// 既定は available_parallelism (取得不能時 4)。旧実装は fold バケットと chunk 毎の
/// AC trie のピーク RSS を抑えるため 4 固定だったが、AC を単一化してファイル走査を
/// 1 回に集約した現構成では worker 毎の保持は fold バケットだけで、コア数分並列でも
/// RSS は問題にならない。低メモリ環境向けに `ASTRO_SIGHT_BATCH_WORKERS` で上書き可能。
fn bounded_worker_count() -> usize {
    std::env::var("ASTRO_SIGHT_BATCH_WORKERS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
        })
}

/// `find_references` / `find_references_batch` 共通の rayon プールを構築する。
/// ワーカー数は物理コア数と `bounded_worker_count()` の小さい方。
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

/// 1 つの AC オートマトンに載せる最大パターン数。既定 100,000。
///
/// aho-corasick 1.1 の AC trie はパターン数にほぼ線形で、実測 5 万パターン ≈ 8MB /
/// 20 万パターン ≈ 30MB (`ascii_case_insensitive` 込み)。通常入力は 1 個の AC に収まり、
/// この上限を超える病的な入力だけ AC を分割してビルドメモリを上限化する。分割しても
/// ファイル走査と parse は 1 回のまま (旧実装のような chunk 毎の全リポ再走査はしない)。
/// `ASTRO_SIGHT_REFS_BATCH_CHUNK` で上書き可能 (旧 chunk 設定との後方互換)。
fn refs_ac_split_size() -> usize {
    std::env::var("ASTRO_SIGHT_REFS_BATCH_CHUNK")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(100_000)
}

/// バッチ検索用の AC 群を構築する。返り値は (グローバル index オフセット, AC) の列で、
/// パターン数が `refs_ac_split_size()` 以下なら常に 1 要素。
fn build_batch_acs(symbol_names: &[String]) -> Result<Vec<(usize, aho_corasick::AhoCorasick)>> {
    let split = refs_ac_split_size();
    let mut acs = Vec::with_capacity(symbol_names.len().div_ceil(split));
    for (chunk_ix, chunk) in symbol_names.chunks(split).enumerate() {
        // AC は ASCII CI で構築: CI 言語 (Xojo/PHP) で case 違いを事前フィルタで
        // 取りこぼさないため。非 CI 言語では多少の false positive (大文字小文字違い)
        // が発生するが、AST 比較で弾く。
        let ac = aho_corasick::AhoCorasick::builder()
            .ascii_case_insensitive(true)
            .build(chunk)
            .map_err(|e| anyhow::anyhow!("Failed to build pattern matcher: {e}"))?;
        acs.push((chunk_ix * split, ac));
    }
    Ok(acs)
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

/// `build_batch_acs` が返す AC 群での事前フィルタ。パターン index は各 AC のオフセットを
/// 加算してグローバル index (symbol_names 上の位置) に揃える。全名出現で早期打ち切り。
fn ac_present_indices_multi(
    acs: &[(usize, aho_corasick::AhoCorasick)],
    source: &[u8],
    num_names: usize,
) -> std::collections::HashSet<usize> {
    let mut present_indices: std::collections::HashSet<usize> = std::collections::HashSet::new();
    'outer: for (offset, ac) in acs {
        for mat in ac.find_overlapping_iter(source) {
            present_indices.insert(offset + mat.pattern().as_usize());
            if present_indices.len() == num_names {
                break 'outer;
            }
        }
    }
    present_indices
}

/// `find_references` の per-file 事前フィルタ。検索開始時に 1 回だけ構築して全ファイルで
/// 共有する。`exact` は case-sensitive 言語用の memmem Finder、`ci` は PHP (関数/メソッド/
/// クラス名が case-insensitive) 用の ASCII CI searcher。旧実装は PHP ファイル毎に
/// source 全文の `to_ascii_lowercase` コピーを確保していた。
struct SingleNamePrefilter<'n> {
    exact: memchr::memmem::Finder<'n>,
    ci: aho_corasick::AhoCorasick,
}

impl<'n> SingleNamePrefilter<'n> {
    fn new(symbol_name: &'n str) -> Result<Self> {
        Ok(Self {
            exact: memchr::memmem::Finder::new(symbol_name.as_bytes()),
            ci: aho_corasick::AhoCorasick::builder()
                .ascii_case_insensitive(true)
                .build([symbol_name])
                .map_err(|e| anyhow::anyhow!("Failed to build pattern matcher: {e}"))?,
        })
    }
}

/// 指定シンボルへの参照をディレクトリ内のファイルから検索する。
/// glob パターン（例: "**/*.rs"）によるフィルタも可能。
pub fn find_references(
    symbol_name: &str,
    dir: &Path,
    glob_pattern: Option<&str>,
) -> Result<Vec<SymbolReference>> {
    Ok(find_references_with_scan(symbol_name, dir, glob_pattern, FileScanOptions::default())?.0)
}

/// Reference search with generated-file omission metadata for user-facing scans.
pub fn find_references_with_scan(
    symbol_name: &str,
    dir: &Path,
    glob_pattern: Option<&str>,
    options: FileScanOptions,
) -> Result<(Vec<SymbolReference>, Option<SkippedFiles>)> {
    let collection = collect_files_scan(dir, glob_pattern, options)?;
    let skipped = collection.skipped(dir);
    let files = collection.files;

    let pool = build_bounded_pool()?;
    let prefilter = SingleNamePrefilter::new(symbol_name)?;

    // per-file Vec を全ファイル分保持せず、worker local の Vec へ直接統合する。
    let mut all_refs: Vec<SymbolReference> = pool.install(|| {
        files
            .into_par_iter()
            .fold(Vec::new, |mut local, path| {
                if let Some(path_str) = path.to_str() {
                    let utf8_path = camino::Utf8Path::new(path_str);
                    if let Ok(mut refs) = find_refs_in_file(symbol_name, utf8_path, &prefilter) {
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

    Ok((all_refs, skipped))
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
fn find_refs_in_file(
    symbol_name: &str,
    path: &camino::Utf8Path,
    prefilter: &SingleNamePrefilter<'_>,
) -> Result<Vec<SymbolReference>> {
    let source = parser::read_file(path)?;

    // ファイル言語を拡張子から先読みし、CI 言語ではバイト事前フィルタを skip
    // (memchr は case-sensitive のため Xojo の `MyVar`/`myvar` 一致を取りこぼす)。
    let ext_lang = LangId::from_path(path).ok();
    let is_ci = ext_lang.is_some_and(|l| l.is_case_insensitive());
    if !is_ci {
        // PHP は関数/メソッド/クラス名が case-insensitive なため、大小無視で事前フィルタして
        // case 違いの参照を取りこぼさない。他の case-sensitive 言語は従来どおり memmem で弾く。
        let present = if ext_lang == Some(LangId::Php) {
            prefilter.ci.is_match(source.as_bytes())
        } else {
            prefilter.exact.find(&source).is_some()
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
/// ディレクトリウォーク・AC 走査・parse はすべて名前数に依らずファイル毎 1 回。
/// 旧実装は名前を chunk (既定 64) に分割して chunk 毎に全ファイルを再読込・再パース
/// しており、名前数に比例して全リポ走査が繰り返されていた (実測: 全ファイルにヒット
/// する名前構成で chunk 2 個 = wall 2 倍)。AC trie は aho-corasick 1.1 でパターン数に
/// ほぼ線形 (実測 5 万パターン ≈ 8MB) のため、単一 AC 化で chunk の根拠は消えている。
/// fold/reduce でワーカー局所バケットに直接統合し、per_file Vec + merged HashMap の
/// 二重保持を回避する。
pub fn find_references_batch(
    symbol_names: &[String],
    dir: &Path,
    glob_pattern: Option<&str>,
) -> Result<std::collections::HashMap<String, Vec<SymbolReference>>> {
    Ok(find_references_batch_with_scan(
        symbol_names,
        dir,
        glob_pattern,
        FileScanOptions::default(),
    )?
    .0)
}

type ReferenceBatchMap = std::collections::HashMap<String, Vec<SymbolReference>>;

/// Batch reference search with generated-file omission metadata.
pub fn find_references_batch_with_scan(
    symbol_names: &[String],
    dir: &Path,
    glob_pattern: Option<&str>,
    options: FileScanOptions,
) -> Result<(ReferenceBatchMap, Option<SkippedFiles>)> {
    use std::collections::HashMap;

    if symbol_names.is_empty() {
        return Ok((HashMap::new(), None));
    }

    let collection = collect_files_scan(dir, glob_pattern, options)?;
    let skipped = collection.skipped(dir);
    let files = collection.files;
    let pool = build_bounded_pool()?;
    let acs = build_batch_acs(symbol_names)?;

    // Angular template scan の前処理 (canonicalize / `is_angular_project` の全 dir 走査 /
    // `collect_component_templates` の全 `.ts` 走査) も 1 回で済ます。
    // 非 Angular リポでは `None` となり、template scan を完全に skip する。
    let angular_ctx =
        crate::engine::angular_template_refs::AngularBatchContext::prepare(dir, glob_pattern);

    // fold/reduce: ワーカーごとに Vec<Vec<SymbolReference>> を持ち、直接統合する。
    let mut buckets: Vec<Vec<SymbolReference>> = pool.install(|| {
        files
            .par_iter()
            .fold(
                || vec![Vec::new(); symbol_names.len()],
                |mut local, path| {
                    let Some(path_str) = path.to_str() else {
                        return local;
                    };
                    let utf8_path = camino::Utf8Path::new(path_str);
                    if let Ok(per_file) =
                        find_refs_batch_in_file_indexed(symbol_names, &acs, utf8_path)
                    {
                        for (ix, mut refs) in per_file.into_iter().enumerate() {
                            local[ix].append(&mut refs);
                        }
                    }
                    local
                },
            )
            .reduce(
                || vec![Vec::new(); symbol_names.len()],
                |mut acc, mut local| {
                    for (acc_refs, local_refs) in acc.iter_mut().zip(local.iter_mut()) {
                        acc_refs.append(local_refs);
                    }
                    acc
                },
            )
    });

    // Angular template バインディング式からの参照を全名まとめて統合する (GitLab #18)。
    if let Some(ctx) = angular_ctx.as_ref() {
        let template_refs =
            crate::engine::angular_template_refs::find_angular_template_references_batch_with_context(
                symbol_names, ctx,
            );
        for (bucket, mut t) in buckets.iter_mut().zip(template_refs) {
            bucket.append(&mut t);
        }
    }

    let mut merged: HashMap<String, Vec<SymbolReference>> =
        HashMap::with_capacity(symbol_names.len());
    for (i, name) in symbol_names.iter().enumerate() {
        let mut refs = std::mem::take(&mut buckets[i]);
        sort_references(&mut refs);
        if !refs.is_empty() {
            merged.insert(name.clone(), refs);
        }
    }

    Ok((merged, skipped))
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
    // shared atomic counters: rayon の fold バケットで `(vec![0; n], vec![0; n])` を
    // worker 毎に確保せず、n × 16 bytes 一定のメモリで全 worker から fetch_add する
    // (dead-code は unique_names が大規模リポジトリで数万〜数十万件に達するため)。
    let prod_counts: Vec<AtomicUsize> = (0..n).map(|_| AtomicUsize::new(0)).collect();
    let test_counts: Vec<AtomicUsize> = (0..n).map(|_| AtomicUsize::new(0)).collect();

    // AC は原則 1 個 (aho-corasick 1.1 で数万パターン ≈ 数 MB、詳細は refs_ac_split_size)。
    // ファイル走査と parse は名前数に依らず 1 回で済む。旧実装は 1024 名 chunk 毎に
    // 全ファイルを再読込・再パースしており、dead-code の名前数に比例して全リポ走査が
    // 繰り返されていた。
    let acs = build_batch_acs(symbol_names)?;

    files.par_iter().for_each(|path| {
        let Some(path_str) = path.to_str() else {
            return;
        };
        let utf8_path = camino::Utf8Path::new(path_str);
        if let Ok(per_file) = count_refs_in_file(symbol_names, &acs, utf8_path) {
            let bucket = if is_test(path) {
                &test_counts
            } else {
                &prod_counts
            };
            for (ix, cnt) in per_file.into_iter().enumerate() {
                if cnt != 0 {
                    bucket[ix].fetch_add(cnt, Ordering::Relaxed);
                }
            }
        }
    });

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

    let name_index = build_name_index(lang_id, symbol_names, &present_indices);

    let matcher = IndexedMatcher {
        name_index: &name_index,
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
/// find_references_batch の fold/reduce から呼ばれる。
pub(crate) fn find_refs_batch_in_file_indexed(
    symbol_names: &[String],
    acs: &[(usize, aho_corasick::AhoCorasick)],
    path: &camino::Utf8Path,
) -> Result<Vec<Vec<SymbolReference>>> {
    let num = symbol_names.len();
    let source = parser::read_file(path)?;

    // マルチパターン事前フィルタ (AC は ASCII CI で構築済、超集合フィルタ)
    let present_indices = ac_present_indices_multi(acs, source.as_bytes(), num);
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

    // 言語別に照合ドメイン別の index を構築する (Xojo は case 折りたたみ、PHP は
    // 関数/メソッド/クラス系の case-insensitive 参照用に folded map を別途持つ)。
    let name_index = build_name_index(lang_id, symbol_names, &present_indices);

    let mut result = vec![Vec::new(); num];
    let matcher = IndexedMatcher {
        name_index: &name_index,
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
    acs: &[(usize, aho_corasick::AhoCorasick)],
    path: &camino::Utf8Path,
) -> Result<Vec<usize>> {
    let num = symbol_names.len();
    let source = parser::read_file(path)?;

    let present_indices = ac_present_indices_multi(acs, source.as_bytes(), num);
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

    // 言語別に照合ドメイン別の index を構築 (Xojo は case 折りたたみ、PHP は関数/メソッド/
    // クラス系の case-insensitive 参照用に folded map を別途持つ)。
    let name_index = build_name_index(lang_id, symbol_names, &present_indices);

    let mut counts = vec![0usize; num];
    let matcher = IndexedMatcher {
        name_index: &name_index,
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
