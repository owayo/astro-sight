use anyhow::Result;
use rayon::prelude::*;
use std::path::Path;
use tree_sitter::Node;

use crate::engine::bash_trap_refs::bash_trap_handler_ref_segments;
use crate::engine::parser;
use crate::engine::phpunit_refs::phpunit_metadata_ref_segments;
use crate::language::{LangId, normalize_identifier};
use crate::models::reference::{RefConfidence, RefKind, SymbolReference};

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

/// 指定シンボルへの参照をディレクトリ内のファイルから検索する。
/// glob パターン（例: "**/*.rs"）によるフィルタも可能。
pub fn find_references(
    symbol_name: &str,
    dir: &Path,
    glob_pattern: Option<&str>,
) -> Result<Vec<SymbolReference>> {
    let files = collect_files(dir, glob_pattern)?;

    let worker_limit = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(bounded_worker_count());
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(worker_limit)
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to build rayon pool: {e}"))?;

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

/// ignore クレートでファイルを収集する（.gitignore 対応）。
pub fn collect_files(dir: &Path, glob_pattern: Option<&str>) -> Result<Vec<std::path::PathBuf>> {
    collect_files_with_excludes(dir, glob_pattern, &[], &[])
}

/// ignore クレートでファイルを収集し、ディレクトリ名またはネガティブ glob で除外する。
///
/// - `excluded_dir_names`: 完全一致するパスセグメント (例: `vendor`, `node_modules`) を
///   含むファイルを除外。軽量な判定用。
/// - `excluded_globs`: `database/migrations/**` のような glob パターン (ワークスペース相対)。
///   内部で `!<pattern>` として `ignore::overrides` に追加し、パッケージパス内の特定サブ
///   ディレクトリだけをピンポイント除外する。
///
/// 両方が空であれば `collect_files(dir, glob)` と同じ挙動。`.gitignore` は常に尊重する。
pub fn collect_files_with_excludes(
    dir: &Path,
    glob_pattern: Option<&str>,
    excluded_dir_names: &[&str],
    excluded_globs: &[&str],
) -> Result<Vec<std::path::PathBuf>> {
    use ignore::WalkBuilder;

    let mut builder = WalkBuilder::new(dir);
    builder.hidden(true).git_ignore(true).git_global(true);

    // glob フィルタと除外 glob を同じ OverrideBuilder にまとめる。
    // ignore::overrides は「ポジティブパターンがある → その中だけ許可 / ネガティブ (`!`
    // 接頭辞) → 除外」なので、glob_pattern が None のときも `**/*` を足してから
    // `!excluded_globs...` を重ねることで「全体許可 + 指定分だけ除外」を実現する。
    if glob_pattern.is_some() || !excluded_globs.is_empty() {
        let mut ob = ignore::overrides::OverrideBuilder::new(dir);
        if let Some(pattern) = glob_pattern {
            ob.add(pattern)?;
        } else if !excluded_globs.is_empty() {
            ob.add("**/*")?;
        }
        for pat in excluded_globs {
            let negated = if pat.starts_with('!') {
                pat.to_string()
            } else {
                format!("!{pat}")
            };
            ob.add(&negated)?;
        }
        builder.overrides(ob.build()?);
    }

    let exclude_generated = !is_generated_exclusion_disabled();
    let mut files = Vec::new();
    for entry in builder.build() {
        let entry = entry?;
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.into_path();
        if !excluded_dir_names.is_empty() {
            // root (`dir`) からの相対パスでセグメント判定する。`dir` が
            // `/private/tmp/test/myrepo` のように親パスに除外セグメント名
            // (例: `test`) を含むと、リポ内全ファイルが誤除外される false negative を防ぐ。
            let rel = path.strip_prefix(dir).unwrap_or(&path);
            if path_has_excluded_segment(rel, excluded_dir_names) {
                continue;
            }
        }
        // ベンダー / IDE 補助 / `@generated` マーカー付きファイルは
        // ノイズ源になるため refs / impact / dead-code から除外する。
        // 例: `vendor/`, `node_modules/`, `_ide_helper.php`, `*.min.js` 等。
        // `ASTRO_SIGHT_NO_GENERATED_EXCLUSION=1` で抑止可能。
        if exclude_generated && is_generated_file(&path) {
            continue;
        }
        // パース可能なファイルのみ対象
        if LangId::from_path(camino::Utf8Path::new(path.to_str().unwrap_or(""))).is_ok() {
            files.push(path);
        } else if path.extension().is_none() && detect_lang_from_shebang(&path).is_some() {
            // 拡張子なしの実行スクリプト (例: `bin/install`) は shebang から言語推定。
            // CLI ツール / ビルドスクリプトで shebang 命名は一般的なので、これを
            // collect_files から落とさない。
            files.push(path);
        }
    }

    Ok(files)
}

/// `ASTRO_SIGHT_NO_GENERATED_EXCLUSION=1` のときだけ generated ファイル除外を抑止する。
fn is_generated_exclusion_disabled() -> bool {
    std::env::var("ASTRO_SIGHT_NO_GENERATED_EXCLUSION")
        .ok()
        .filter(|v| !v.is_empty() && v != "0")
        .is_some()
}

/// IDE 補助 / minified / `@generated` マーカー付きのファイル単位 generated 判定。
///
/// **vendor/node_modules 等のディレクトリ単位の除外はこの関数では行わない**。
/// それらは dead-code の `--include-vendor` のような opt-in がある呼び出し側で
/// `excluded_dir_names` 経由で個別に制御される。
///
/// 以下のいずれかに該当する場合 true を返す:
/// - ファイル名が Laravel IDE Helper 系 (`_ide_helper.php`, `_ide_helper_*.php`, `_lighthouse_ide_helper.php`)
/// - ファイル名が minified / bundled (`*.min.js`, `*.min.css`, `*.bundle.js`)
/// - ファイル先頭 4KB に `@generated`, `DO NOT EDIT`, `Code generated by`,
///   `This file is auto-generated`, `automatically generated` のいずれかを含む
fn is_generated_file(path: &Path) -> bool {
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        if name.starts_with("_ide_helper") && name.ends_with(".php") {
            return true;
        }
        if name == "_lighthouse_ide_helper.php" {
            return true;
        }
        if name.ends_with(".min.js") || name.ends_with(".min.css") || name.ends_with(".bundle.js") {
            return true;
        }
    }
    has_generated_marker(path)
}

/// ファイル先頭 4KB に generated マーカーが含まれるかを判定する。
///
/// I/O コスト軽減のため最大 4KB のみ読む。マーカー文字列は UTF-8 妥当性を問わず
/// バイト列としてマッチさせる (memchr::memmem)。読めない場合は false を返す
/// (パーミッション欠落等で誤って除外しない)。
fn has_generated_marker(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf = [0u8; 4096];
    let Ok(n) = file.read(&mut buf) else {
        return false;
    };
    let head = &buf[..n];
    memchr::memmem::find(head, b"@generated").is_some()
        || memchr::memmem::find(head, b"DO NOT EDIT").is_some()
        || memchr::memmem::find(head, b"Code generated by").is_some()
        || memchr::memmem::find(head, b"This file is auto-generated").is_some()
        || memchr::memmem::find(head, b"automatically generated").is_some()
}

/// 拡張子なしファイルの先頭 shebang 行から言語を判定する。
///
/// I/O コスト軽減のため最大 4KB だけ読み、最初の 2 byte が `#!` でなければ即時 None。
/// 実行ビット等は判定せず、shebang の有無だけで言語推定可能かを決める。
fn detect_lang_from_shebang(path: &Path) -> Option<LangId> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut buf = [0u8; 4096];
    let n = file.read(&mut buf).ok()?;
    if n < 2 || &buf[..2] != b"#!" {
        return None;
    }
    let line_end = buf[..n].iter().position(|&b| b == b'\n').unwrap_or(n);
    let first_line = std::str::from_utf8(&buf[..line_end]).ok()?;
    LangId::from_shebang(first_line)
}

/// パスのいずれかの中間ディレクトリ名が除外対象と完全一致するかを判定する。
fn path_has_excluded_segment(path: &Path, excluded: &[&str]) -> bool {
    path.components().any(|c| match c.as_os_str().to_str() {
        Some(name) => excluded.contains(&name),
        None => false,
    })
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

    let mut refs = Vec::new();
    let definition_kinds = definition_node_kinds(lang_id);
    let target = normalize_identifier(lang_id, symbol_name);
    // 同一 source 内で複数 hit する関数 (例: util / to_string) でも line context 取得を O(1) に。
    let line_index = LineIndex::new(&source);
    collect_identifier_refs(
        root,
        &source,
        &line_index,
        target.as_ref(),
        path.as_str(),
        definition_kinds,
        lang_id,
        &mut refs,
    );

    Ok(refs)
}

/// lexer-only ファイル向けの参照検索。
///
/// identifier トークン位置を列挙し、定義ヘッダ行 (Class/Sub/Function 等) と
/// 一致するものを Definition、それ以外を Reference として返す。
fn find_refs_via_lexer(
    symbol_name: &str,
    source: &[u8],
    path: &camino::Utf8Path,
    lexer_lang: crate::language::LexerLang,
) -> Vec<SymbolReference> {
    use crate::engine::lexer;

    // 定義ヘッダ位置 (行番号) のセット。lexer profile 経由で抽出。
    let def_lines: std::collections::HashSet<usize> = lexer::extract_symbols(source, lexer_lang)
        .iter()
        .filter(|s| {
            // 大小無視で名前一致するもののみ抽出 (Xojo は case-insensitive)。
            let profile = lexer::profile_for(lexer_lang);
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
fn count_refs_in_file_via_lexer(
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
fn find_refs_batch_via_lexer(
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

/// 指定行 (0-indexed) のコンテキスト行を取得する。lexer fallback 経路用の
/// 軽量実装 (tree-sitter の Node API に依存しない)。1 ファイル内で複数行参照する
/// 場合は `extract_line_context_bytes_indexed` を使い `LineIndex` を共有する。
/// 本体経路は indexed 版を直接呼ぶため、この単発 wrapper はテスト専用。
#[cfg(test)]
fn extract_line_context_bytes(source: &[u8], line_0idx: usize) -> String {
    extract_line_context_bytes_indexed(source, &LineIndex::new(source), line_0idx)
}

/// `LineIndex` を共有して指定行を O(1) で取り出す lexer 経路向け実装。
/// 1 ファイル内で M 件の参照を処理する場合、O(M × filesize) → O(M + filesize) に削減する。
fn extract_line_context_bytes_indexed(
    source: &[u8],
    index: &LineIndex,
    line_0idx: usize,
) -> String {
    // minified/生成コードの巨大行によるメモリ・出力爆発を防ぐため 256B で切り詰める
    // (tree-sitter 経路の extract_line_context と同じ上限)。
    const MAX_CTX: usize = 256;
    let Some((start, end)) = index.line_bounds(source.len(), line_0idx) else {
        return String::new();
    };
    let line = std::str::from_utf8(&source[start..end])
        .unwrap_or("")
        .trim_end_matches('\r');
    if line.len() <= MAX_CTX {
        line.to_string()
    } else {
        format!("{}...", &line[..line.floor_char_boundary(MAX_CTX)])
    }
}

/// 1 ファイルの改行位置 (0-indexed 行頭 byte offset) をキャッシュする索引。
/// `extract_line_context*` を O(filesize) の per-call 走査から O(1) の lookup に
/// 切り替えるため、ファイル単位で 1 度だけ構築して visitor 群に貸し出す。
pub(crate) struct LineIndex {
    /// `line_starts[i]` は 0-indexed の i 行目の先頭 byte offset。終端番兵は持たない。
    line_starts: Vec<u32>,
}

impl LineIndex {
    pub(crate) fn new(source: &[u8]) -> Self {
        // 1 行平均 ~32B 想定で初期 capacity を見積もる
        let mut line_starts = Vec::with_capacity(source.len() / 32 + 1);
        line_starts.push(0u32);
        // 100MB のファイルサイズ上限 (parser::MAX_FILE_SIZE) ≪ u32::MAX のため u32 で十分。
        for nl in memchr::memchr_iter(b'\n', source) {
            // 改行直後の byte offset が次行先頭
            line_starts.push((nl + 1) as u32);
        }
        Self { line_starts }
    }

    /// 指定行の本体 byte 範囲 `[start, end)` を返す。末尾の `\n` は含めない。
    /// 行が存在しない場合は `None`。
    pub(crate) fn line_bounds(&self, source_len: usize, row: usize) -> Option<(usize, usize)> {
        let start = *self.line_starts.get(row)? as usize;
        if start > source_len {
            return None;
        }
        // 次の行頭から `\n` 1 バイトを差し引いた位置が現行の末尾。
        // 最終行 (番兵なし) は source 末尾まで。
        let end = self
            .line_starts
            .get(row + 1)
            .map(|&n| (n as usize).saturating_sub(1).min(source_len))
            .unwrap_or(source_len);
        Some((start, end))
    }
}

/// AST を再帰走査し、指定シンボル名に一致する identifier ノードを収集する。
/// `symbol_name` は言語に応じて正規化済みであることが前提。
/// `line_index` はファイル単位で 1 度だけ構築し、参照件数 M に対して O(M × filesize)
/// だった `extract_line_context` 累積コストを O(M + filesize) に抑える。
#[allow(clippy::too_many_arguments)]
fn collect_identifier_refs(
    node: Node<'_>,
    source: &[u8],
    line_index: &LineIndex,
    symbol_name: &str,
    path: &str,
    definition_kinds: &[&str],
    lang_id: LangId,
    refs: &mut Vec<SymbolReference>,
) {
    if is_identifier_kind(node.kind())
        && let Ok(text) = node.utf8_text(source)
        && ident_ref_matches(lang_id, node, text, symbol_name)
        && !(lang_id == LangId::Rust && is_rust_struct_field_non_callable(node))
    {
        let is_def = is_definition_context(node, definition_kinds, lang_id);
        let context = extract_line_context_indexed(source, line_index, node.start_position().row);

        refs.push(SymbolReference {
            path: path.to_string(),
            line: node.start_position().row,
            column: node.start_position().column,
            context: Some(context),
            kind: Some(if is_def {
                RefKind::Definition
            } else {
                RefKind::Reference
            }),
            confidence: None,
        });
    }

    // Rust の serde 属性文字列値を識別子参照として扱う。
    for (seg, row, col) in rust_attr_string_ref_segments(node, source, lang_id) {
        if seg_ref_matches(lang_id, seg, symbol_name) {
            refs.push(SymbolReference {
                path: path.to_string(),
                line: row,
                column: col,
                context: Some(extract_line_context_indexed(source, line_index, row)),
                kind: Some(RefKind::Reference),
                confidence: None,
            });
        }
    }

    // bash の `trap '<handler>' SIG` 内の handler 文字列から関数参照を抽出する。
    for (seg, row, col) in bash_trap_handler_ref_segments(node, source, lang_id) {
        if seg_ref_matches(lang_id, &seg, symbol_name) {
            refs.push(SymbolReference {
                path: path.to_string(),
                line: row,
                column: col,
                context: Some(extract_line_context_indexed(source, line_index, row)),
                kind: Some(RefKind::Reference),
                confidence: None,
            });
        }
    }

    // PHPUnit の DocBlock (`@dataProvider` / `@depends`) や PHP attribute
    // (`#[DataProvider('name')]` 等) 経由で参照される method を ref として扱う。
    for (seg, row, col) in phpunit_metadata_ref_segments(node, source, lang_id) {
        if seg_ref_matches(lang_id, &seg, symbol_name) {
            refs.push(SymbolReference {
                path: path.to_string(),
                line: row,
                column: col,
                context: Some(extract_line_context_indexed(source, line_index, row)),
                kind: Some(RefKind::Reference),
                confidence: None,
            });
        }
    }

    // PHP の callable array `[Foo::class, 'method']` の string literal を ref として扱う (N3)。
    if let Some((method, row, col)) = php_callable_array_method_segment(node, source, lang_id)
        && seg_ref_matches(lang_id, method, symbol_name)
    {
        refs.push(SymbolReference {
            path: path.to_string(),
            line: row,
            column: col,
            context: Some(extract_line_context_indexed(source, line_index, row)),
            kind: Some(RefKind::Reference),
            confidence: None,
        });
    }

    // PHP の文字列 callable `'Class@method'` / `Class::class . '@method'` を ref として扱う (N4)。
    if let Some((method, row, col)) = php_string_callable_method_segment(node, source, lang_id)
        && seg_ref_matches(lang_id, method, symbol_name)
    {
        refs.push(SymbolReference {
            path: path.to_string(),
            line: row,
            column: col,
            context: Some(extract_line_context_indexed(source, line_index, row)),
            kind: Some(RefKind::Reference),
            confidence: None,
        });
    }

    // 子ノードを再帰走査
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_identifier_refs(
            child,
            source,
            line_index,
            symbol_name,
            path,
            definition_kinds,
            lang_id,
            refs,
        );
    }
}

/// この identifier ノードが定義コンテキストにあるかを判定する。
fn is_definition_context(node: Node<'_>, definition_kinds: &[&str], lang_id: LangId) -> bool {
    if lang_id == LangId::Ruby {
        return is_ruby_definition_context(node);
    }
    if lang_id == LangId::Php {
        return is_php_definition_context(node);
    }
    if matches!(
        lang_id,
        LangId::Typescript | LangId::Tsx | LangId::Javascript
    ) {
        return is_js_ts_definition_context(node, definition_kinds);
    }
    if lang_id == LangId::Zig {
        return is_zig_definition_context(node, definition_kinds);
    }

    // C/C++: type_definition の declarator と enumerator の name のみを Definition とみなす。
    // typedef の元型 (`typedef MYSQL MyConn;` の MYSQL) や enumerator の value 式内の識別子
    // (`FOO = BAR` の BAR) は参照として扱う。これにより enumerator / typedef alias の宣言行を
    // 参照と二重計上せず、宣言名経由の liveness 判定 (Issue #11/#12) を成立させる。
    if matches!(lang_id, LangId::C | LangId::Cpp)
        && let Some(is_def) = cpp_typedef_enum_definition_context(node)
    {
        return is_def;
    }

    if let Some(parent) = node.parent() {
        // 親ノードが定義ノードかチェック
        if definition_kinds.contains(&parent.kind()) {
            return true;
        }
        // 祖父ノードもチェック（例: function_declarator > identifier）
        if let Some(grandparent) = parent.parent()
            && definition_kinds.contains(&grandparent.kind())
        {
            return true;
        }
    }
    false
}

/// C/C++ の type_definition / enumerator に属する識別子の Definition 判定。
///
/// - `type_definition` の `declarator` フィールド (typedef alias 名) → Definition
/// - `enumerator` の `name` フィールド (列挙子名) → Definition
/// - 上記以外 (typedef の元型、enumerator の value 式内識別子) → 参照 (Some(false))
/// - type_definition / enumerator のいずれにも属さない → None (汎用判定へ委譲)
///
/// enumerator / typedef alias の宣言行を参照と二重計上せず、かつ宣言名 (列挙子名 / alias 名)
/// 経由の参照を liveness 判定に使えるようにするために使う (Issue #11/#12)。
fn cpp_typedef_enum_definition_context(node: Node<'_>) -> Option<bool> {
    let mut cur = node;
    while let Some(parent) = cur.parent() {
        match parent.kind() {
            "type_definition" => {
                // 全 declarator の alias 名 leaf を集め、node がそのいずれかと一致すれば
                // Definition。`typedef int (*H)(MYSQL);` の MYSQL (元型参照) や複数 declarator
                // (`typedef S A, *B;`) も正しく扱い、declarator 配下の型参照は Reference に倒す
                // (codex 指摘 1/2)。
                let is_alias = typedef_alias_name_nodes(parent)
                    .iter()
                    .any(|leaf| leaf.id() == node.id());
                return Some(is_alias);
            }
            "enumerator" => {
                let is_name = parent
                    .child_by_field_name("name")
                    .is_some_and(|n| n.id() == node.id());
                return Some(is_name);
            }
            // 型/関数の境界に達したら type_definition / enumerator の外。
            "function_definition"
            | "struct_specifier"
            | "class_specifier"
            | "union_specifier"
            | "enum_specifier"
            | "field_declaration_list"
            | "compound_statement"
            | "translation_unit"
            | "preproc_def" => {
                return None;
            }
            _ => {}
        }
        cur = parent;
    }
    None
}

/// type_definition の全 declarator フィールドから alias 名の leaf ノードを集める。
/// `typedef struct S {} A, *B;` のように複数 declarator がある場合は全 alias 名を返す。
fn typedef_alias_name_nodes<'a>(type_definition: Node<'a>) -> Vec<Node<'a>> {
    let mut leaves = Vec::new();
    let mut cursor = type_definition.walk();
    for decl in type_definition.children_by_field_name("declarator", &mut cursor) {
        if let Some(leaf) = typedef_declarator_leaf(decl) {
            leaves.push(leaf);
        }
    }
    leaves
}

/// declarator (type_identifier / pointer_declarator / function_declarator 等) から
/// alias 名の leaf identifier を取り出す。pointer/array/function declarator を剥がして
/// 最終的な名前ノードを返す。
fn typedef_declarator_leaf(decl: Node<'_>) -> Option<Node<'_>> {
    match decl.kind() {
        "type_identifier" | "identifier" => Some(decl),
        _ => decl
            .child_by_field_name("declarator")
            .and_then(typedef_declarator_leaf),
    }
}

/// JS/TS/TSX: 識別子が「宣言の `name` フィールド」であるときだけ `Definition` とみなす。
///
/// 単純な parent/grandparent 走査では `function parseExcel(): ExcelParseResult {}` の
/// `ExcelParseResult` (戻り値型) や `class A extends B {}` の `B` が grandparent
/// `function_declaration` / `class_declaration` 等にぶら下がって def と誤判定される。
/// これにより dead-code 判定で「型が ref されているのに def しか見つからない」状況が
/// 発生する (例: `excel-service.ts` で戻り値型 `ExcelParseResult` が dead 扱いになる)。
/// PHP と同じく `name` フィールドの一致を要求し、return_type / extends_clause 等の中の
/// 識別子は ref として分類する。
fn is_js_ts_definition_context(node: Node<'_>, definition_kinds: &[&str]) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };

    // parent が定義ノード: name フィールド一致を要求
    if definition_kinds.contains(&parent.kind()) {
        if let Some(name_node) = parent.child_by_field_name("name")
            && name_node.id() == node.id()
        {
            return true;
        }
        return false;
    }

    // grandparent が定義ノード: parent 経由で name フィールドに到達するときのみ def 扱い
    // (例: `variable_declarator > identifier` の identifier は def、
    //      `function_declaration > return_type > type_identifier` は ref)
    if let Some(grandparent) = parent.parent()
        && definition_kinds.contains(&grandparent.kind())
        && let Some(name_node) = grandparent.child_by_field_name("name")
        && (name_node.id() == node.id() || name_node.id() == parent.id())
    {
        return true;
    }
    false
}

/// Zig: 宣言の「名前位置」にある identifier だけを `Definition` とみなす。
///
/// tree-sitter-zig の AST では:
/// - `variable_declaration` は `name` フィールドが無く、最初の子 identifier が変数名
/// - `function_declaration` は `name`/`type`/`body` フィールドあり (戻り値型は `type`)
/// - `test_declaration` は最初の identifier/string が テスト名
/// - `struct_declaration` / `enum_declaration` 等は `name` フィールドあり
///
/// 単純な parent/grandparent 走査では `const Foo = bar()` の `bar` (右辺) や
/// `fn foo() ReturnType { ... }` の `ReturnType` (戻り値型) が def 誤判定される。
/// 各定義種別ごとに「名前位置」を厳密に判定し、それ以外の identifier は ref として返す。
fn is_zig_definition_context(node: Node<'_>, definition_kinds: &[&str]) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if !definition_kinds.contains(&parent.kind()) {
        return false;
    }

    // 1. name フィールドが定義されている種別 (function_declaration, struct_declaration,
    //    enum_declaration, union_declaration) は name 一致を要求
    if let Some(name_node) = parent.child_by_field_name("name") {
        return name_node.id() == node.id();
    }

    // 2. variable_declaration / test_declaration は最初の identifier (or string) 子が
    //    名前位置。それ以降の identifier は ref として扱う。
    if matches!(parent.kind(), "variable_declaration" | "test_declaration") {
        let mut cursor = parent.walk();
        for child in parent.children(&mut cursor) {
            if matches!(child.kind(), "identifier" | "string") {
                return child.id() == node.id();
            }
        }
    }

    false
}

/// PHP: 識別子が「宣言の `name` フィールド」であるときだけ `Definition` とみなす。
///
/// 単純な parent/grandparent 走査では `class Derived extends AbstractBase` の
/// `AbstractBase` や `implements InterfaceX` の `InterfaceX` が grandparent
/// `class_declaration` にぶら下がって def と誤判定され、継承ツリーを経由した
/// 参照がすべて 0 件になる (dead-code が基底 class / interface を大量に FP とする根因)。
/// field_name が "name" のものだけを定義と数え、`base_clause` / `class_interface_clause` /
/// `use_declaration` 等の中の識別子は ref として分類する。
fn is_php_definition_context(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        "function_definition"
        | "class_declaration"
        | "method_declaration"
        | "interface_declaration"
        | "enum_declaration"
        | "trait_declaration" => parent
            .child_by_field_name("name")
            .is_some_and(|name| name.id() == node.id()),
        _ => false,
    }
}

/// PHP の `name` ノードが case-insensitive 照合すべき文脈にあるか判定する。
///
/// PHP は関数名・メソッド名・クラス/interface/trait/enum 名が case-insensitive だが、
/// 変数・プロパティ・定数は case-sensitive。誤って定数・プロパティ・変数を case-fold
/// しないよう、ホワイトリスト方式で「関数/メソッド/クラス系の名前」だけ true を返す。
fn php_name_is_case_insensitive(node: Node<'_>) -> bool {
    // 定義側 (method/function/class/interface/trait/enum の name フィールド)
    if is_php_definition_context(node) {
        return true;
    }
    let Some(parent) = node.parent() else {
        return false;
    };
    // qualified_name (`\App\Foo` 等) の末尾 name は、namespace prefix (namespace_name 配下)
    // を除き qualified_name 全体を実効ノードとして、その親文脈で判定する。
    // namespace_name 配下の name (App / Repository) は namespace セグメントなので _ に落ちて exact。
    if parent.kind() == "qualified_name" {
        return match parent.parent() {
            Some(grand) => php_ref_context_is_case_insensitive(grand, parent),
            None => false,
        };
    }
    php_ref_context_is_case_insensitive(parent, node)
}

/// 参照ノード `effective` (name または qualified_name) が親 `parent` の文脈で
/// case-insensitive 照合対象 (関数/メソッド/クラス系の名前) かを判定する。
fn php_ref_context_is_case_insensitive(parent: Node<'_>, effective: Node<'_>) -> bool {
    let is_field = |field: &str| {
        parent
            .child_by_field_name(field)
            .is_some_and(|n| n.id() == effective.id())
    };
    // scope 位置判定: tree-sitter-php は scoped_call_expression には field "scope" を付けるが、
    // class_constant_access_expression / scoped_property_access_expression は positional
    // (field 名なし) のため、field を優先しつつ named_child(0) に fallback する。
    let is_scope = || {
        parent.child_by_field_name("scope").map_or_else(
            || {
                parent
                    .named_child(0)
                    .is_some_and(|s| s.id() == effective.id())
            },
            |s| s.id() == effective.id(),
        )
    };
    match parent.kind() {
        // $x->method() のメソッド名。member_access_expression ($x->prop) は _ に落ちて exact。
        "member_call_expression" => is_field("name"),
        // func() のグローバル/名前空間関数名
        "function_call_expression" => is_field("function"),
        // Foo::method() — scope(クラス名) も name(メソッド名) も case-fold
        // (self/static/parent は relative_scope ノードのため scope は名前ノードにならない)
        "scoped_call_expression" => is_scope() || is_field("name"),
        // Foo::CONST — scope(クラス名)は case-fold、定数 name は exact。
        // ただし trait adaptation (`A::foo insteadof B` / `A::foo as bar`) 配下の name は
        // trait メソッド名なので case-fold する。
        "class_constant_access_expression" => {
            if is_scope() {
                true
            } else {
                matches!(
                    parent.parent().map(|g| g.kind()),
                    Some("use_instead_of_clause") | Some("use_as_clause")
                )
            }
        }
        // Foo::$prop — scope(クラス名)のみ case-fold。静的プロパティ ($prop) は variable_name で別ノード。
        "scoped_property_access_expression" => is_scope(),
        // new Foo() / extends Foo / implements Iface / trait use の直接子はクラス系名。
        // 引数等の name は parent が arguments 等になるため誤巻き込みしない。
        "object_creation_expression"
        | "base_clause"
        | "class_interface_clause"
        | "use_declaration" => true,
        // 名前空間 use: クラス / 関数 import は case-insensitive、定数 import (`use const`) は
        // PHP 定数が case-sensitive なため exact のままにする。
        "namespace_use_clause" => !php_namespace_use_is_const(parent),
        // 型ヒント `Foo $x` の named_type 内 (variable_name 内の name は _ に落ちて exact)
        "named_type" => true,
        // trait adaptation の trait 名 / 別名 (`use A, B { B::foo insteadof A; A::bar as baz; }`)
        "use_instead_of_clause" | "use_as_clause" => true,
        _ => false,
    }
}

/// PHP の名前空間 use 文が `use const ...` (定数 import) かを判定する。
///
/// 定数は case-sensitive のため case-fold しない。`use` (クラス import) / `use function`
/// (関数 import) は case-insensitive。group use (`use App\{const FOO, Bar, function baz}`) の
/// 個別修飾子と、`use const App\{...}` のグループ全体修飾子の両方に対応する。
/// `const` / `function` は anonymous keyword node として現れる。
fn php_namespace_use_is_const(clause: Node<'_>) -> bool {
    // group use の各 clause は自身の先頭に const / function キーワードを持つ場合がある。
    let mut cursor = clause.walk();
    for child in clause.children(&mut cursor) {
        match child.kind() {
            "const" => return true,
            "function" => return false,
            _ => {}
        }
    }
    // 単純 use / group 全体の修飾子は namespace_use_declaration 直下にある。
    let mut current = clause.parent();
    while let Some(n) = current {
        if n.kind() == "namespace_use_declaration" {
            let mut decl_cursor = n.walk();
            for child in n.children(&mut decl_cursor) {
                match child.kind() {
                    "const" => return true,
                    "function" => return false,
                    // clause / group 本体に到達したら宣言レベルの修飾子はない
                    "namespace_use_clause" | "namespace_use_group" => break,
                    _ => {}
                }
            }
            return false;
        }
        current = n.parent();
    }
    false
}

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
fn build_name_to_ix<'a>(
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

fn is_ruby_definition_context(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };

    match parent.kind() {
        "method" | "singleton_method" => parent
            .child_by_field_name("name")
            .is_some_and(|name| name.id() == node.id()),
        "assignment" => parent
            .child_by_field_name("left")
            .is_some_and(|left| left.id() == node.id()),
        "class" | "module" => parent
            .child_by_field_name("name")
            .is_some_and(|name| name.id() == node.id()),
        "scope_resolution" => {
            let is_name = parent
                .child_by_field_name("name")
                .is_some_and(|name| name.id() == node.id());
            if !is_name {
                return false;
            }

            if let Some(grandparent) = parent.parent() {
                match grandparent.kind() {
                    "assignment" => grandparent
                        .child_by_field_name("left")
                        .is_some_and(|left| left.id() == parent.id()),
                    "class" | "module" => grandparent
                        .child_by_field_name("name")
                        .is_some_and(|name| name.id() == parent.id()),
                    _ => false,
                }
            } else {
                false
            }
        }
        _ => false,
    }
}

/// 言語ごとの定義ノード種別を返す。
/// 静的スライスを返すことで毎回の Vec アロケーションを回避する。
fn definition_node_kinds(lang_id: LangId) -> &'static [&'static str] {
    match lang_id {
        LangId::Rust => &[
            "function_item",
            "function_signature_item", // trait メソッド宣言（ボディなし）
            "struct_item",
            "enum_item",
            "trait_item",
            "impl_item",
            "const_item",
            "static_item",
            "type_item",
            "mod_item",
        ],
        LangId::C => &["function_definition", "struct_specifier", "enum_specifier"],
        LangId::Cpp => &[
            "function_definition",
            "struct_specifier",
            "class_specifier",
            "enum_specifier",
            "namespace_definition",
        ],
        LangId::Python => &["function_definition", "class_definition"],
        LangId::Javascript => &[
            "function_declaration",
            "class_declaration",
            "method_definition",
            "variable_declarator",
        ],
        LangId::Typescript | LangId::Tsx => &[
            "function_declaration",
            "class_declaration",
            "method_definition",
            "interface_declaration",
            "type_alias_declaration",
            "enum_declaration",
            "variable_declarator",
        ],
        LangId::Go => &[
            "package_clause",
            "function_declaration",
            "method_declaration",
            "type_spec",
        ],
        LangId::Php => &[
            "function_definition",
            "class_declaration",
            "method_declaration",
            "interface_declaration",
            "enum_declaration",
            "trait_declaration",
        ],
        LangId::Java => &[
            "method_declaration",
            "class_declaration",
            "interface_declaration",
            "enum_declaration",
        ],
        LangId::Kotlin => &[
            "function_declaration",
            "class_declaration",
            "object_declaration",
        ],
        LangId::Swift => &[
            "function_declaration",
            "class_declaration",
            "protocol_declaration",
        ],
        LangId::CSharp => &[
            "namespace_declaration",
            "method_declaration",
            "class_declaration",
            "struct_declaration",
            "interface_declaration",
            "enum_declaration",
        ],
        LangId::Bash => &["function_definition"],
        LangId::Ruby => &[
            "method",
            "singleton_method",
            "class",
            "module",
            "assignment",
        ],
        LangId::Zig => &[
            "function_declaration",
            "variable_declaration",
            "test_declaration",
            "struct_declaration",
            "enum_declaration",
            "union_declaration",
        ],
        LangId::Xojo => &[
            "class_declaration",
            "module_declaration",
            "interface_declaration",
            "structure_declaration",
            "enum_declaration",
            "sub_declaration",
            "function_declaration",
            "constructor_declaration",
            "destructor_declaration",
            "event_declaration",
            "delegate_declaration",
            "simple_property_declaration",
            "computed_property_declaration",
            "const_declaration",
            "field_declaration",
            "declare_declaration",
        ],
    }
}

/// identifier ノードかどうかを判定する。
fn is_identifier_kind(kind: &str) -> bool {
    matches!(
        kind,
        "identifier"
            | "type_identifier"
            | "field_identifier"
            | "property_identifier"
            | "simple_identifier"
            | "namespace_identifier"
            | "package_identifier"
            | "name"
            | "qualified_name"
            | "word"
            | "constant"
    )
}

/// Rust の構造体フィールド系ノードを参照として扱うべきでないかを判定する。
///
/// `pub fn redact()` のような関数名と struct field 名 (`pub redact: bool`) が衝突した場合、
/// `cfg.output.redact` のフィールドアクセスや struct 宣言/初期化が関数の呼び出し位置として
/// 誤マッチして impact 分析にノイズを生む (Issue: 2026-05-21-redact-impact-triage)。
///
/// 関数ではないことが構造的に明らかな以下のケースを除外する:
/// - `field_declaration` の field_identifier (struct のフィールド宣言)
/// - `field_initializer` の field 側 field_identifier (`Config { redact: ... }`)
/// - `field_pattern` の field_identifier (destructuring `let Cfg { redact: v } = ...`)
/// - `shorthand_field_initializer` 配下の identifier (`Config { redact }`)
/// - `field_expression` の field_identifier で、祖先 `call_expression.function` でないもの
///   (純粋なフィールドアクセス `obj.redact`)
///
/// 一方、メソッド呼び出し (`obj.method()`) の `method` 部は `field_identifier` ノードだが
/// 親 `field_expression` がさらに親 `call_expression` の `function` フィールドに位置するため
/// 関数参照として残す。
fn is_rust_struct_field_non_callable(node: Node<'_>) -> bool {
    match node.kind() {
        "field_identifier" => {
            let Some(parent) = node.parent() else {
                return false;
            };
            match parent.kind() {
                // `pub redact: bool` の name
                "field_declaration" => true,
                // `Config { redact: ... }` の field 部
                "field_initializer" => parent
                    .child_by_field_name("field")
                    .is_some_and(|n| n.id() == node.id()),
                // `let Cfg { redact: v } = ...` の destructuring 中の field name 部
                // (`field_pattern` の field_identifier は常に name 役割で、pattern 部は別ノード)
                "field_pattern" => true,
                // `obj.redact` または `obj.redact()` の field 部
                "field_expression" => {
                    let Some(grand) = parent.parent() else {
                        // 祖先なし → 純粋なフィールドアクセスとして除外
                        return true;
                    };
                    // method call (`obj.method()`) の `method` 部は関数参照として残す
                    let is_method_call = grand.kind() == "call_expression"
                        && grand
                            .child_by_field_name("function")
                            .is_some_and(|n| n.id() == parent.id());
                    !is_method_call
                }
                _ => false,
            }
        }
        // shorthand: `Config { redact }` の `redact`
        "identifier" => node
            .parent()
            .is_some_and(|p| p.kind() == "shorthand_field_initializer"),
        _ => false,
    }
}

/// Rust の属性引数で文字列値を識別子/パス参照として解釈すべきキー。
/// serde 系の `#[serde(serialize_with = "path::to::fn")]` 形式を想定する。
const RUST_ATTR_STRING_REF_KEYS: &[&str] = &[
    "serialize_with",
    "deserialize_with",
    "with",
    "skip_serializing_if",
    "try_from",
    "from",
    "into",
];

/// `string_content` ノードが Rust の serde 系属性値として現れるかを判定する。
/// 構造: `attribute > token_tree > identifier "=" string_literal > string_content`
fn is_rust_attribute_ref_string(node: Node<'_>, source: &[u8]) -> bool {
    let Some(string_literal) = node.parent() else {
        return false;
    };
    if string_literal.kind() != "string_literal" {
        return false;
    }
    let Some(token_tree) = string_literal.parent() else {
        return false;
    };
    if token_tree.kind() != "token_tree" {
        return false;
    }

    // token_tree の直下兄弟で `identifier "=" string_literal` の並びを検出する。
    let mut cursor = token_tree.walk();
    let mut prev_prev: Option<Node> = None;
    let mut prev: Option<Node> = None;
    for child in token_tree.children(&mut cursor) {
        if child.id() == string_literal.id() {
            let Some(eq) = prev else {
                return false;
            };
            if eq.kind() != "=" {
                return false;
            }
            let Some(key) = prev_prev else {
                return false;
            };
            if key.kind() != "identifier" {
                return false;
            }
            let Ok(key_text) = key.utf8_text(source) else {
                return false;
            };
            return RUST_ATTR_STRING_REF_KEYS.contains(&key_text);
        }
        prev_prev = prev;
        prev = Some(child);
    }
    false
}

/// "Option::is_none" を [("Option", 0), ("is_none", 8)] のように (segment, byte offset) で分割する。
fn split_path_segments(text: &str) -> Vec<(&str, usize)> {
    let mut results = Vec::new();
    let mut offset = 0usize;
    for seg in text.split("::") {
        if !seg.is_empty() {
            results.push((seg, offset));
        }
        offset += seg.len() + 2; // "::"
    }
    results
}

/// Rust 属性の string_content から (segment, row, col) を列挙する。
/// 非 Rust やパターンに合わない場合は空 Vec を返す。
fn rust_attr_string_ref_segments<'a>(
    node: Node<'_>,
    source: &'a [u8],
    lang_id: LangId,
) -> Vec<(&'a str, usize, usize)> {
    if lang_id != LangId::Rust || node.kind() != "string_content" {
        return Vec::new();
    }
    if !is_rust_attribute_ref_string(node, source) {
        return Vec::new();
    }
    let Ok(text) = node.utf8_text(source) else {
        return Vec::new();
    };
    let base = node.start_position();
    split_path_segments(text)
        .into_iter()
        .map(|(seg, off)| (seg, base.row, base.column + off))
        .collect()
}

/// PHP の callable array `[<Class>::class, '<method>']` パターンから
/// `<method>` の文字列を method reference として返す (N3)。
///
/// Laravel 7+ 推奨の Route 記法 `Route::get('/path', [Foo::class, 'bar'])` や
/// `[Foo::class, 'method']` で `'method'` 部分が string literal となるため、
/// tree-sitter の identifier ノードでは捕捉できない。誤検出を避けるため、
/// 第1要素が `Foo::class` (= `class_constant_access_expression` の右辺が
/// `class` キーワード) であり、第2要素が単独の string literal で
/// 中身が PHP 識別子文法に合致する場合のみ ref として認める。
fn php_callable_array_method_segment<'a>(
    node: Node<'_>,
    source: &'a [u8],
    lang_id: LangId,
) -> Option<(&'a str, usize, usize)> {
    if lang_id != LangId::Php || node.kind() != "array_creation_expression" {
        return None;
    }
    let mut cursor = node.walk();
    let elements: Vec<Node> = node
        .children(&mut cursor)
        .filter(|c| c.kind() == "array_element_initializer")
        .collect();
    if elements.len() != 2 {
        return None;
    }

    // 第1要素: class_constant_access_expression で右辺が `class` キーワード
    let first = elements[0];
    let mut fc = first.walk();
    let first_inner = first.children(&mut fc).next()?;
    if first_inner.kind() != "class_constant_access_expression" {
        return None;
    }
    let mut cc = first_inner.walk();
    let has_class_kw = first_inner
        .children(&mut cc)
        .any(|c| c.kind() == "name" && c.utf8_text(source) == Ok("class"));
    if !has_class_kw {
        return None;
    }

    // 第2要素: string / encapsed_string literal
    let second = elements[1];
    let mut sc = second.walk();
    let str_node = second
        .children(&mut sc)
        .find(|c| c.kind() == "string" || c.kind() == "encapsed_string")?;
    let raw = str_node.utf8_text(source).ok()?;
    let trimmed = raw.trim_matches(|c: char| c == '\'' || c == '"');
    if !is_php_identifier(trimmed) {
        return None;
    }
    let pos = str_node.start_position();
    // 引用符の次の文字を method 名の開始位置として登録する
    Some((trimmed, pos.row, pos.column.saturating_add(1)))
}

/// PHP の識別子文法 `[A-Za-z_][A-Za-z0-9_]*` に合致するかを判定する。
fn is_php_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// PHP string literal が Laravel 互換の callable 表記 `Class@method` / `@method`
/// (concat 連結の右辺) を含んでいれば、`method` 部分を ref として返す (N4)。
///
/// 対象構文:
/// 1. 純粋文字列 `'ClassName@handler'` / `'\\Fully\\Qualified\\Name@handler'`
/// 2. 連結 `ClassName::class . '@handler'` の右辺 string (class_part が空)
///
/// 誤検出対策:
/// - method 部分は PHP 識別子 (2 文字以上、英小文字または `_` で始まる)
/// - class 部分が非空の場合、名前空間 `\\` 区切りで各セグメントが 2 文字以上 + 先頭大文字
///   (英小文字始まりの場合はメール/単語の可能性があるため reject)
/// - class 部分が空の場合、親が `binary_expression` (`.` 演算子) で左辺が `X::class`
///   (`class_constant_access_expression`) の場合のみ認める
/// - double-quoted (encapsed_string) は補間で構造が崩れるため対象外
fn php_string_callable_method_segment<'a>(
    node: Node<'_>,
    source: &'a [u8],
    lang_id: LangId,
) -> Option<(&'a str, usize, usize)> {
    if lang_id != LangId::Php || node.kind() != "string" {
        return None;
    }
    let raw = node.utf8_text(source).ok()?;
    if raw.len() < 2 {
        return None;
    }
    let bytes = raw.as_bytes();
    let first = bytes[0];
    let last = bytes[raw.len() - 1];
    if (first != b'\'' && first != b'"') || first != last {
        return None;
    }
    let body = &raw[1..raw.len() - 1];

    let at_pos = body.find('@')?;
    let class_part = &body[..at_pos];
    let method_part = &body[at_pos + 1..];

    if !is_php_method_name(method_part) {
        return None;
    }
    let class_ok = if class_part.is_empty() {
        is_parent_class_const_concat(node, source)
    } else {
        is_php_class_path_strict(class_part)
    };
    if !class_ok {
        return None;
    }

    let start = node.start_position();
    // quote 1 byte + class_part bytes + '@' 1 byte。column は tree-sitter の仕様上
    // byte offset 相当なので、method 先頭の byte 位置として足し合わせる。
    let byte_offset = 1 + class_part.len() + 1;
    Some((
        method_part,
        start.row,
        start.column.saturating_add(byte_offset),
    ))
}

/// N4 method 部分用: PHP 識別子 かつ 英小文字/`_` で始まる、かつ 2 文字以上。
/// `'P@ssw0rd'` (class_part='P', method_part='ssw0rd') を弾くため method 側は厳しめにしない
/// 代わりに class_part 側で 1 文字を reject する。ここは英識別子であれば広めに許容する。
fn is_php_method_name(s: &str) -> bool {
    if s.len() < 2 {
        return false;
    }
    let mut chars = s.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_lowercase() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// N4 class 部分用: 名前空間 `\\` 区切りで各セグメントが先頭大文字 + 2 文字以上 + 識別子。
/// 先頭 `\\` の absolute namespace プレフィクスも許容する。
fn is_php_class_path_strict(s: &str) -> bool {
    let s = s.strip_prefix('\\').unwrap_or(s);
    if s.is_empty() {
        return false;
    }
    for part in s.split('\\') {
        if part.len() < 2 {
            return false;
        }
        let mut chars = part.chars();
        let first = chars.next().unwrap();
        if !first.is_ascii_uppercase() {
            return false;
        }
        if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return false;
        }
    }
    true
}

/// N4 parent check: `node` が `X::class . node` 形式の concat 右辺であれば true。
fn is_parent_class_const_concat(node: Node<'_>, source: &[u8]) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if parent.kind() != "binary_expression" {
        return false;
    }
    // operator field: tree-sitter-php では binary_expression の `operator` は子ノード
    // として現れる。field 名で取れなくても子 token で `.` を探す。
    let mut cursor = parent.walk();
    let op_is_dot = parent.children(&mut cursor).any(|c| {
        // operator トークンは kind = "." になる (tree-sitter-php)
        c.kind() == "." && c.utf8_text(source) == Ok(".")
    });
    if !op_is_dot {
        return false;
    }
    // node が parent の右側にいるか確認: 親の children で node より前に
    // class_constant_access_expression が存在することを検証する。
    let mut cur2 = parent.walk();
    let mut seen_class_const = false;
    let mut node_is_right = false;
    for c in parent.children(&mut cur2) {
        if c.id() == node.id() {
            node_is_right = seen_class_const;
            break;
        }
        if c.kind() == "class_constant_access_expression" && is_class_class_expr(c, source) {
            seen_class_const = true;
        }
    }
    node_is_right
}

/// `X::class` 形式の class_constant_access_expression かを判定。
fn is_class_class_expr(node: Node<'_>, source: &[u8]) -> bool {
    if node.kind() != "class_constant_access_expression" {
        return false;
    }
    let mut c = node.walk();
    node.children(&mut c)
        .any(|child| child.kind() == "name" && child.utf8_text(source) == Ok("class"))
}

/// 指定行のソース行をコンテキストとして抽出する。
/// 後方互換のため `LineIndex` を内部で作る単発呼び出し用。1 ファイル内で複数行を
/// 取り出す場合は `extract_line_context_indexed` を使い `LineIndex` を共有する。
/// 本体経路は indexed 版を直接呼ぶため、この単発 wrapper はテスト専用。
#[cfg(test)]
fn extract_line_context(source: &[u8], row: usize) -> String {
    extract_line_context_indexed(source, &LineIndex::new(source), row)
}

/// `LineIndex` を共有して指定行を O(1) で取り出す tree-sitter 経路向け実装。
/// minified/生成コードの巨大行によるメモリ爆発を防ぐため 256B で切り詰める。
/// 1 ファイル内で M 件の参照を処理する場合、O(M × filesize) → O(M + filesize) に削減する。
fn extract_line_context_indexed(source: &[u8], index: &LineIndex, row: usize) -> String {
    const MAX_CTX: usize = 256;
    let Some((start, end)) = index.line_bounds(source.len(), row) else {
        return String::new();
    };
    // 必要な範囲のみ UTF-8 変換する（失敗時は空コンテキストを返す）
    let line = std::str::from_utf8(&source[start..end])
        .unwrap_or("")
        .trim();
    if line.len() <= MAX_CTX {
        line.to_string()
    } else {
        // UTF-8 境界で安全に切り詰める
        let truncated = &line[..line.floor_char_boundary(MAX_CTX)];
        format!("{truncated}...")
    }
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

    // rayon のワーカー数を上限付きにする。ワーカー毎に `Vec<Vec<SymbolReference>>` の
    // fold バケットが生成されるため、大規模リポジトリではワーカー数 × 参照件数に比例して
    // ピーク RSS が線形増大する。
    // 物理コア数と上限のうち小さい方を採用し、バケット総量を押さえつつ並列性を維持する。
    let worker_limit = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(bounded_worker_count());
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(worker_limit)
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to build rayon pool: {e}"))?;

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

/// dead-code 判定用 (test-only 分類版): 各シンボルの非 Definition 参照件数を
/// production と test それぞれ別カウントで返す。
///
/// `is_test` predicate は呼び出し側から渡す (例: `is_test_path` / ディレクトリセグメント判定)。
/// 戻り値は `HashMap<symbol_name, (production_count, test_count)>`。
pub fn count_non_definition_refs_split<F>(
    symbol_names: &[String],
    dir: &Path,
    glob_pattern: Option<&str>,
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

    let files = collect_files(dir, glob_pattern)?;

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
    use std::collections::HashSet;

    let num = symbol_names.len();
    let source = parser::read_file(path)?;

    let mut present_indices: HashSet<usize> = HashSet::new();
    for mat in ac.find_overlapping_iter(source.as_bytes()) {
        present_indices.insert(mat.pattern().as_usize());
        if present_indices.len() == num {
            break;
        }
    }
    if present_indices.is_empty() {
        return Ok(());
    }

    let (tree, lang_id) = parser::parse_file(path, &source)?;
    let root = tree.root_node();
    let definition_kinds = definition_node_kinds(lang_id);

    let name_to_ix = build_name_to_ix(lang_id, symbol_names, &present_indices);

    // 同一 source 内で複数 hit する識別子でも line context 取得を O(1) に。
    let line_index = LineIndex::new(&source);
    collect_refs_and_defs_indexed_cb(
        root,
        &source,
        &line_index,
        &name_to_ix,
        definition_kinds,
        lang_id,
        visitor,
    );
    Ok(())
}

/// `visit_refs_and_defs_in_file_cb` が内部で呼び出す訪問者 trait。
/// Xojo の case-insensitive 多重 index (`name_to_ix[key]` が `Vec<usize>`) や
/// Rust attribute 文字列内参照の場合も、ヒットしたすべての sym_ix について
/// 1 回ずつ `on_ref` が呼ばれる。
pub(crate) trait RefVisitor {
    /// `confidence` は receiver-aware 解析の確信度。
    /// Phase 3 (PHP receiver-aware) より前は常に `RefConfidence::ExactOwner` を渡す。
    fn on_ref(
        &mut self,
        sym_ix: u32,
        line: usize,
        column: usize,
        context: &str,
        is_def: bool,
        confidence: RefConfidence,
    );
}

/// `visit_refs_and_defs_in_file_cb` 用の AST 再帰走査。Identifier と Rust attribute 文字列
/// 参照を発見したら `visitor.on_ref` を直接呼び、`Vec<SymbolReference>` を一切生成しない。
/// `line_index` はファイル単位で 1 度だけ構築し、参照件数 M に対して O(M × filesize)
/// だった line context 取得を O(M + filesize) に抑える。
fn collect_refs_and_defs_indexed_cb<V: RefVisitor>(
    node: Node<'_>,
    source: &[u8],
    line_index: &LineIndex,
    name_to_ix: &std::collections::HashMap<std::borrow::Cow<'_, str>, Vec<usize>>,
    definition_kinds: &[&str],
    lang_id: LangId,
    visitor: &mut V,
) {
    if is_identifier_kind(node.kind())
        && let Ok(text) = node.utf8_text(source)
        && let Some(indices) = name_to_ix.get(&node_ref_key(lang_id, node, text))
        && !(lang_id == LangId::Rust && is_rust_struct_field_non_callable(node))
    {
        let is_def = is_definition_context(node, definition_kinds, lang_id);
        let context = extract_line_context_indexed(source, line_index, node.start_position().row);
        let line = node.start_position().row;
        let column = node.start_position().column;
        // Phase 3 で PHP method ref を receiver-aware に分類する。
        // それまでは PHP も含めて ExactOwner を渡し、挙動互換を保つ。
        let confidence = classify_method_ref_confidence(node, source, lang_id, is_def);
        for &ix in indices {
            visitor.on_ref(ix as u32, line, column, &context, is_def, confidence);
        }
    }

    for (seg, row, col) in rust_attr_string_ref_segments(node, source, lang_id) {
        if let Some(indices) = name_to_ix.get(&seg_ref_key(lang_id, seg)) {
            let context = extract_line_context_indexed(source, line_index, row);
            for &ix in indices {
                visitor.on_ref(
                    ix as u32,
                    row,
                    col,
                    &context,
                    false,
                    RefConfidence::ExactOwner,
                );
            }
        }
    }

    // bash の `trap '<handler>' SIG` 内の handler 文字列から関数参照を抽出する。
    for (seg, row, col) in bash_trap_handler_ref_segments(node, source, lang_id) {
        if let Some(indices) = name_to_ix.get(&seg_ref_key(lang_id, &seg)) {
            let context = extract_line_context_indexed(source, line_index, row);
            for &ix in indices {
                visitor.on_ref(
                    ix as u32,
                    row,
                    col,
                    &context,
                    false,
                    RefConfidence::ExactOwner,
                );
            }
        }
    }

    // PHPUnit の DocBlock / attribute 経由で参照される method を ref として扱う。
    for (seg, row, col) in phpunit_metadata_ref_segments(node, source, lang_id) {
        if let Some(indices) = name_to_ix.get(&seg_ref_key(lang_id, &seg)) {
            let context = extract_line_context_indexed(source, line_index, row);
            for &ix in indices {
                visitor.on_ref(
                    ix as u32,
                    row,
                    col,
                    &context,
                    false,
                    RefConfidence::ExactOwner,
                );
            }
        }
    }

    // PHP の callable array `[Foo::class, 'method']` を ref として扱う (N3)。
    // collect_identifier_refs_indexed / count_identifier_refs と挙動を揃え、
    // impact streaming Pass でも同じ結果を返すようにする。
    if let Some((method, row, col)) = php_callable_array_method_segment(node, source, lang_id)
        && let Some(indices) = name_to_ix.get(&seg_ref_key(lang_id, method))
    {
        let context = extract_line_context_indexed(source, line_index, row);
        for &ix in indices {
            visitor.on_ref(
                ix as u32,
                row,
                col,
                &context,
                false,
                RefConfidence::ExactOwner,
            );
        }
    }

    // PHP の文字列 callable `'Class@method'` / `Class::class . '@method'` を ref とする (N4)。
    if let Some((method, row, col)) = php_string_callable_method_segment(node, source, lang_id)
        && let Some(indices) = name_to_ix.get(&seg_ref_key(lang_id, method))
    {
        let context = extract_line_context_indexed(source, line_index, row);
        for &ix in indices {
            visitor.on_ref(
                ix as u32,
                row,
                col,
                &context,
                false,
                RefConfidence::ExactOwner,
            );
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_refs_and_defs_indexed_cb(
            child,
            source,
            line_index,
            name_to_ix,
            definition_kinds,
            lang_id,
            visitor,
        );
    }
}

/// receiver-aware な method ref 確信度判定 (Phase 3 で PHP に拡張予定)。
/// - 定義ノード: ExactOwner (定義そのもの)
/// - PHP の `member_call_expression` 直下の name node:
///   - `Foo::bar()` → ExactOwner (`scoped_call_expression`)
///   - `[Foo::class, 'bar']` → ExactOwner (callable array、別経路で処理)
///   - `$x->bar()` で `@var Foo $x` 解析できれば InferredOwner、無ければ BareNameOnly
/// - PHP 以外: 既存挙動維持のため ExactOwner を返す
fn classify_method_ref_confidence(
    node: Node<'_>,
    source: &[u8],
    lang_id: LangId,
    is_def: bool,
) -> RefConfidence {
    if is_def {
        return RefConfidence::ExactOwner;
    }
    if !matches!(lang_id, LangId::Php) {
        return RefConfidence::ExactOwner;
    }
    classify_php_method_ref(node, source)
}

/// PHP の identifier ノードから method ref の確信度を判定する。
///
/// - `scoped_call_expression` (`Foo::bar()`) の name 子 → `ExactOwner`
/// - `member_call_expression` (`$x->bar()`) の name 子 → `InferredOwner` または `BareNameOnly`
///   - 同関数本体内で `@var Foo $x` または `Foo $x` のパラメータ型注釈があれば InferredOwner
///   - それ以外は BareNameOnly
/// - `function_call_expression` (`bar()`) など receiver なし呼び出しは ExactOwner 扱い
///   (グローバル関数 / namespace 関数として全箇所が caller になりうる)
/// - 上記以外 (定義 / クラス名等) は ExactOwner
fn classify_php_method_ref(node: Node<'_>, source: &[u8]) -> RefConfidence {
    let Some(parent) = node.parent() else {
        return RefConfidence::ExactOwner;
    };
    match parent.kind() {
        // Foo::bar() — class scope が明示されているので ExactOwner
        "scoped_call_expression" | "scoped_property_access_expression" => RefConfidence::ExactOwner,
        // $x->bar() — receiver の型を最低限調査
        "member_call_expression" | "member_access_expression" => {
            php_member_call_inferred_or_bare(parent, source)
        }
        _ => RefConfidence::ExactOwner,
    }
}

/// `$x->bar()` の receiver `$x` の型を簡易判定する。
///
/// 同一関数本体内で以下のいずれかが見つかれば InferredOwner、なければ BareNameOnly:
/// - `Foo $x` パラメータ型注釈 (parameter declaration with type)
/// - `@var Foo $x` PHPDoc コメント (簡易テキスト検索)
/// - `$x = new Foo(...)` 代入
///
/// 詳細な型推論は行わず、見つかったら InferredOwner にする保守的判定。
/// `$this->bar()` は InferredOwner (enclosing class が判明している)。
fn php_member_call_inferred_or_bare(call_expr: Node<'_>, source: &[u8]) -> RefConfidence {
    // receiver ノードを取得 (member_call_expression の object フィールド)
    let receiver = call_expr.child_by_field_name("object");
    let Some(receiver) = receiver else {
        return RefConfidence::BareNameOnly;
    };

    // $this->bar(): enclosing class が型として推論可能
    if let Ok(rcv_text) = receiver.utf8_text(source)
        && rcv_text == "$this"
    {
        return RefConfidence::InferredOwner;
    }

    // 変数名を抽出 ($x → "x")
    let var_name = match receiver.utf8_text(source) {
        Ok(t) if t.starts_with('$') => &t[1..],
        _ => return RefConfidence::BareNameOnly,
    };

    // 同一関数スコープを上方向に探索
    let Some(func_body) = enclosing_function_body(call_expr) else {
        return RefConfidence::BareNameOnly;
    };

    // (1) パラメータ型注釈 / (2) `@var` コメント / (3) `new ClassName()` 代入を探す
    if php_scope_has_inferable_var_type(func_body, source, var_name) {
        RefConfidence::InferredOwner
    } else {
        RefConfidence::BareNameOnly
    }
}

/// 与えられたノードを内包する関数 / メソッド本体ノードを返す。
fn enclosing_function_body<'a>(node: Node<'a>) -> Option<Node<'a>> {
    let mut current = node.parent();
    while let Some(n) = current {
        match n.kind() {
            "method_declaration"
            | "function_definition"
            | "function_static_declaration"
            | "anonymous_function_creation_expression"
            | "arrow_function" => {
                return n.child_by_field_name("body");
            }
            _ => current = n.parent(),
        }
    }
    None
}

/// 関数本体内に変数 `$var_name` の型推論材料があるかを判定する。
fn php_scope_has_inferable_var_type(body: Node<'_>, source: &[u8], var_name: &str) -> bool {
    let var_marker = format!("${var_name}");
    php_scope_has_inferable_var_type_recursive(body, source, &var_marker)
}

fn php_scope_has_inferable_var_type_recursive(
    node: Node<'_>,
    source: &[u8],
    var_marker: &str,
) -> bool {
    match node.kind() {
        // simple_parameter / parameter は子に type と name を持つ
        "simple_parameter" | "property_promotion_parameter" => {
            let has_type = node.child_by_field_name("type").is_some();
            let name_match = node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
                .is_some_and(|t| t == var_marker);
            if has_type && name_match {
                return true;
            }
        }
        // $x = new Foo(...) の代入式
        "assignment_expression" => {
            let lhs_match = node
                .child_by_field_name("left")
                .and_then(|n| n.utf8_text(source).ok())
                .is_some_and(|t| t == var_marker);
            let rhs_is_object_creation = node
                .child_by_field_name("right")
                .is_some_and(|n| n.kind() == "object_creation_expression");
            if lhs_match && rhs_is_object_creation {
                return true;
            }
        }
        // PHPDoc `@var Foo $x` を含む block-level コメント
        "comment" => {
            if let Ok(text) = node.utf8_text(source)
                && text.contains("@var")
                && text.contains(var_marker)
            {
                // 雑だが「@var Foo $x」が同じ comment 内にあれば InferredOwner と判定
                return true;
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if php_scope_has_inferable_var_type_recursive(child, source, var_marker) {
            return true;
        }
    }
    false
}

/// 単一ファイル内で複数シンボルの参照を index ベースの Vec に格納する。
/// find_references_batch の fold/reduce および impact analyze の streaming Pass から呼ばれる。
pub(crate) fn find_refs_batch_in_file_indexed(
    symbol_names: &[String],
    ac: &aho_corasick::AhoCorasick,
    path: &camino::Utf8Path,
) -> Result<Vec<Vec<SymbolReference>>> {
    use std::collections::HashSet;

    let num = symbol_names.len();
    let source = parser::read_file(path)?;

    // マルチパターン事前フィルタ (AC は ASCII CI で構築済、超集合フィルタ)
    let mut present_indices: HashSet<usize> = HashSet::new();
    for mat in ac.find_overlapping_iter(source.as_bytes()) {
        present_indices.insert(mat.pattern().as_usize());
        if present_indices.len() == num {
            break;
        }
    }

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

    // 同一 source 内で複数 hit する識別子でも line context 取得を O(1) に。
    let line_index = LineIndex::new(&source);
    let mut result = vec![Vec::new(); num];
    collect_identifier_refs_indexed(
        root,
        &source,
        &line_index,
        &name_to_ix,
        path.as_str(),
        definition_kinds,
        lang_id,
        &mut result,
    );

    Ok(result)
}

/// AST を再帰走査し、シンボル index ベースの Vec に参照を格納する。
/// CI 言語（Xojo）で正規化後キーが衝突する場合でも全 index に参照を配るため、
/// 値は `Vec<usize>` を受け取る。
/// `line_index` はファイル単位で 1 度だけ構築し、参照件数 M に対して O(M × filesize)
/// だった line context 取得を O(M + filesize) に抑える。
#[allow(clippy::too_many_arguments)]
fn collect_identifier_refs_indexed(
    node: Node<'_>,
    source: &[u8],
    line_index: &LineIndex,
    name_to_ix: &std::collections::HashMap<std::borrow::Cow<'_, str>, Vec<usize>>,
    path: &str,
    definition_kinds: &[&str],
    lang_id: LangId,
    refs: &mut [Vec<SymbolReference>],
) {
    if is_identifier_kind(node.kind())
        && let Ok(text) = node.utf8_text(source)
        && let Some(indices) = name_to_ix.get(&node_ref_key(lang_id, node, text))
        && !(lang_id == LangId::Rust && is_rust_struct_field_non_callable(node))
    {
        let is_def = is_definition_context(node, definition_kinds, lang_id);
        let context = extract_line_context_indexed(source, line_index, node.start_position().row);
        let line = node.start_position().row;
        let column = node.start_position().column;
        let kind = if is_def {
            RefKind::Definition
        } else {
            RefKind::Reference
        };

        for &ix in indices {
            refs[ix].push(SymbolReference {
                path: path.to_string(),
                line,
                column,
                context: Some(context.clone()),
                kind: Some(kind),
                confidence: None,
            });
        }
    }

    // Rust の serde 属性文字列値を識別子参照として扱う。
    for (seg, row, col) in rust_attr_string_ref_segments(node, source, lang_id) {
        if let Some(indices) = name_to_ix.get(&seg_ref_key(lang_id, seg)) {
            let context = extract_line_context_indexed(source, line_index, row);
            for &ix in indices {
                refs[ix].push(SymbolReference {
                    path: path.to_string(),
                    line: row,
                    column: col,
                    context: Some(context.clone()),
                    kind: Some(RefKind::Reference),
                    confidence: None,
                });
            }
        }
    }

    // bash の `trap '<handler>' SIG` 内の handler 文字列から関数参照を抽出する。
    for (seg, row, col) in bash_trap_handler_ref_segments(node, source, lang_id) {
        if let Some(indices) = name_to_ix.get(&seg_ref_key(lang_id, &seg)) {
            let context = extract_line_context_indexed(source, line_index, row);
            for &ix in indices {
                refs[ix].push(SymbolReference {
                    path: path.to_string(),
                    line: row,
                    column: col,
                    context: Some(context.clone()),
                    kind: Some(RefKind::Reference),
                    confidence: None,
                });
            }
        }
    }

    // PHPUnit の DocBlock / attribute 経由で参照される method を ref として扱う。
    for (seg, row, col) in phpunit_metadata_ref_segments(node, source, lang_id) {
        if let Some(indices) = name_to_ix.get(&seg_ref_key(lang_id, &seg)) {
            let context = extract_line_context_indexed(source, line_index, row);
            for &ix in indices {
                refs[ix].push(SymbolReference {
                    path: path.to_string(),
                    line: row,
                    column: col,
                    context: Some(context.clone()),
                    kind: Some(RefKind::Reference),
                    confidence: None,
                });
            }
        }
    }

    // PHP の callable array `[Foo::class, 'method']` の string literal を ref として扱う (N3)。
    if let Some((method, row, col)) = php_callable_array_method_segment(node, source, lang_id)
        && let Some(indices) = name_to_ix.get(&seg_ref_key(lang_id, method))
    {
        let context = extract_line_context_indexed(source, line_index, row);
        for &ix in indices {
            refs[ix].push(SymbolReference {
                path: path.to_string(),
                line: row,
                column: col,
                context: Some(context.clone()),
                kind: Some(RefKind::Reference),
                confidence: None,
            });
        }
    }

    // PHP の文字列 callable `'Class@method'` / `Class::class . '@method'` を ref として扱う (N4)。
    if let Some((method, row, col)) = php_string_callable_method_segment(node, source, lang_id)
        && let Some(indices) = name_to_ix.get(&seg_ref_key(lang_id, method))
    {
        let context = extract_line_context_indexed(source, line_index, row);
        for &ix in indices {
            refs[ix].push(SymbolReference {
                path: path.to_string(),
                line: row,
                column: col,
                context: Some(context.clone()),
                kind: Some(RefKind::Reference),
                confidence: None,
            });
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_identifier_refs_indexed(
            child,
            source,
            line_index,
            name_to_ix,
            path,
            definition_kinds,
            lang_id,
            refs,
        );
    }
}

/// 単一ファイル内の非 Definition 参照件数をカウントする（SymbolReference を確保しない）。
fn count_refs_in_file(
    symbol_names: &[String],
    ac: &aho_corasick::AhoCorasick,
    path: &camino::Utf8Path,
) -> Result<Vec<usize>> {
    use std::collections::HashSet;

    let num = symbol_names.len();
    let source = parser::read_file(path)?;

    let mut present_indices: HashSet<usize> = HashSet::new();
    for mat in ac.find_overlapping_iter(source.as_bytes()) {
        present_indices.insert(mat.pattern().as_usize());
        if present_indices.len() == num {
            break;
        }
    }

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
    count_identifier_refs(
        root,
        &source,
        &name_to_ix,
        definition_kinds,
        lang_id,
        &mut counts,
    );

    Ok(counts)
}

/// AST を再帰走査し、非 Definition 参照の件数のみカウントする。
/// CI 言語 (Xojo) で正規化後キーが衝突する場合でも全 index にカウントを配るため、
/// 値は `Vec<usize>` を受け取る。
fn count_identifier_refs(
    node: Node<'_>,
    source: &[u8],
    name_to_ix: &std::collections::HashMap<std::borrow::Cow<'_, str>, Vec<usize>>,
    definition_kinds: &[&str],
    lang_id: LangId,
    counts: &mut [usize],
) {
    if is_identifier_kind(node.kind())
        && let Ok(text) = node.utf8_text(source)
        && let Some(ixs) = name_to_ix.get(&node_ref_key(lang_id, node, text))
        && !is_definition_context(node, definition_kinds, lang_id)
        && !(lang_id == LangId::Rust && is_rust_struct_field_non_callable(node))
    {
        for &ix in ixs {
            counts[ix] += 1;
        }
    }

    // Rust の serde 属性文字列値を非 Definition 参照としてカウントする。
    for (seg, _row, _col) in rust_attr_string_ref_segments(node, source, lang_id) {
        if let Some(ixs) = name_to_ix.get(&seg_ref_key(lang_id, seg)) {
            for &ix in ixs {
                counts[ix] += 1;
            }
        }
    }

    // bash の `trap '<handler>' SIG` 内の handler 文字列から関数参照をカウントする。
    for (seg, _row, _col) in bash_trap_handler_ref_segments(node, source, lang_id) {
        if let Some(ixs) = name_to_ix.get(&seg_ref_key(lang_id, &seg)) {
            for &ix in ixs {
                counts[ix] += 1;
            }
        }
    }

    // PHPUnit の DocBlock / attribute 経由で参照される method をカウントする。
    for (seg, _row, _col) in phpunit_metadata_ref_segments(node, source, lang_id) {
        if let Some(ixs) = name_to_ix.get(&seg_ref_key(lang_id, &seg)) {
            for &ix in ixs {
                counts[ix] += 1;
            }
        }
    }

    // PHP の callable array `[Foo::class, 'method']` の string literal を ref とする (N3)。
    if let Some((method, _row, _col)) = php_callable_array_method_segment(node, source, lang_id)
        && let Some(ixs) = name_to_ix.get(&seg_ref_key(lang_id, method))
    {
        for &ix in ixs {
            counts[ix] += 1;
        }
    }

    // PHP の文字列 callable `'Class@method'` / `Class::class . '@method'` を ref とする (N4)。
    if let Some((method, _row, _col)) = php_string_callable_method_segment(node, source, lang_id)
        && let Some(ixs) = name_to_ix.get(&seg_ref_key(lang_id, method))
    {
        for &ix in ixs {
            counts[ix] += 1;
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        count_identifier_refs(child, source, name_to_ix, definition_kinds, lang_id, counts);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PHP の callable array `[Class::class, 'method']` で string ノードの中身が
    /// method ref として返されることを検証 (N3 unit-level)。
    #[test]
    fn php_callable_array_method_segment_extracts_method_string() {
        let source = b"<?php\nclass C {\n    public function h() { $x = [C::class, 'foo']; return $x; }\n}\n";
        let path = camino::Utf8Path::new("dummy.php");
        let tree = parser::parse_source(source, LangId::Php).unwrap();
        let lang_id = LangId::Php;
        let _ = path; // silence unused warning
        // 再帰で array_creation_expression を探す
        fn find_array<'t>(n: tree_sitter::Node<'t>) -> Option<tree_sitter::Node<'t>> {
            if n.kind() == "array_creation_expression" {
                return Some(n);
            }
            let mut c = n.walk();
            for child in n.children(&mut c) {
                if let Some(found) = find_array(child) {
                    return Some(found);
                }
            }
            None
        }
        let arr = find_array(tree.root_node()).expect("array_creation_expression must exist");
        let seg = php_callable_array_method_segment(arr, source, lang_id);
        assert!(
            seg.is_some(),
            "[C::class, 'foo'] should yield a method segment, got None"
        );
        let (m, _row, _col) = seg.unwrap();
        assert_eq!(m, "foo");
    }

    /// 第1要素が `Class::class` でない場合は ref として認めない (誤検出防止)
    #[test]
    fn php_callable_array_method_segment_rejects_non_class_const() {
        // [1, 'foo'] や ['foo', 'bar'] は callable array ではない
        let source = b"<?php\nfunction f() { $x = [1, 'foo']; return $x; }\n";
        let tree = parser::parse_source(source, LangId::Php).unwrap();
        let lang_id = LangId::Php;
        fn find_array<'t>(n: tree_sitter::Node<'t>) -> Option<tree_sitter::Node<'t>> {
            if n.kind() == "array_creation_expression" {
                return Some(n);
            }
            let mut c = n.walk();
            for child in n.children(&mut c) {
                if let Some(found) = find_array(child) {
                    return Some(found);
                }
            }
            None
        }
        let arr = find_array(tree.root_node()).expect("array_creation_expression must exist");
        assert!(php_callable_array_method_segment(arr, source, lang_id).is_none());
    }

    /// PHP 文字列 callable `'Cls@method'` 形式で method 部分が抽出されることを検証 (N4)。
    #[test]
    fn php_string_callable_method_segment_extracts_pure_string() {
        let source = b"<?php\nfunction f() { return 'Controller@handle'; }\n";
        let tree = parser::parse_source(source, LangId::Php).unwrap();
        fn find_string<'t>(
            n: tree_sitter::Node<'t>,
            target: &str,
            source: &[u8],
        ) -> Option<tree_sitter::Node<'t>> {
            if n.kind() == "string" && n.utf8_text(source).ok() == Some(target) {
                return Some(n);
            }
            let mut c = n.walk();
            for child in n.children(&mut c) {
                if let Some(found) = find_string(child, target, source) {
                    return Some(found);
                }
            }
            None
        }
        let s = find_string(tree.root_node(), "'Controller@handle'", source)
            .expect("string must exist");
        let seg = php_string_callable_method_segment(s, source, LangId::Php);
        assert!(
            seg.is_some(),
            "'Controller@handle' should yield method segment"
        );
        let (m, _r, _c) = seg.unwrap();
        assert_eq!(m, "handle");
    }

    /// PHP `Cls::class . '@method'` concat 右辺 string から method 部分が抽出されることを検証 (N4)。
    #[test]
    fn php_string_callable_method_segment_extracts_concat_segment() {
        let source = b"<?php\nclass C {}\n$x = C::class . '@handler';\n";
        let tree = parser::parse_source(source, LangId::Php).unwrap();
        fn find_string<'t>(
            n: tree_sitter::Node<'t>,
            target: &str,
            source: &[u8],
        ) -> Option<tree_sitter::Node<'t>> {
            if n.kind() == "string" && n.utf8_text(source).ok() == Some(target) {
                return Some(n);
            }
            let mut c = n.walk();
            for child in n.children(&mut c) {
                if let Some(found) = find_string(child, target, source) {
                    return Some(found);
                }
            }
            None
        }
        let s = find_string(tree.root_node(), "'@handler'", source).expect("string must exist");
        let seg = php_string_callable_method_segment(s, source, LangId::Php);
        assert!(seg.is_some(), "Cls::class . '@handler' should match");
        let (m, _r, _c) = seg.unwrap();
        assert_eq!(m, "handler");
    }

    /// メール風文字列は method ref として抽出しない (誤検出防止)
    #[test]
    fn php_string_callable_method_segment_rejects_email_like() {
        let source = b"<?php\n$x = 'user@example.com';\n";
        let tree = parser::parse_source(source, LangId::Php).unwrap();
        fn find_string<'t>(n: tree_sitter::Node<'t>) -> Option<tree_sitter::Node<'t>> {
            if n.kind() == "string" {
                return Some(n);
            }
            let mut c = n.walk();
            for child in n.children(&mut c) {
                if let Some(found) = find_string(child) {
                    return Some(found);
                }
            }
            None
        }
        let s = find_string(tree.root_node()).expect("string must exist");
        assert!(php_string_callable_method_segment(s, source, LangId::Php).is_none());
    }

    /// `P@ssw0rd` のようなパスワード風文字列は class 部分が 1 文字で reject される
    #[test]
    fn php_string_callable_method_segment_rejects_short_class_part() {
        let source = b"<?php\n$x = 'P@ssw0rd';\n";
        let tree = parser::parse_source(source, LangId::Php).unwrap();
        fn find_string<'t>(n: tree_sitter::Node<'t>) -> Option<tree_sitter::Node<'t>> {
            if n.kind() == "string" {
                return Some(n);
            }
            let mut c = n.walk();
            for child in n.children(&mut c) {
                if let Some(found) = find_string(child) {
                    return Some(found);
                }
            }
            None
        }
        let s = find_string(tree.root_node()).expect("string must exist");
        assert!(php_string_callable_method_segment(s, source, LangId::Php).is_none());
    }

    /// 引数単独の `'@method'` (concat 親ではない) は reject
    #[test]
    fn php_string_callable_method_segment_rejects_bare_at_method() {
        let source = b"<?php\nfunction f($x) {} f('@handler');\n";
        let tree = parser::parse_source(source, LangId::Php).unwrap();
        fn find_string<'t>(n: tree_sitter::Node<'t>) -> Option<tree_sitter::Node<'t>> {
            if n.kind() == "string" {
                return Some(n);
            }
            let mut c = n.walk();
            for child in n.children(&mut c) {
                if let Some(found) = find_string(child) {
                    return Some(found);
                }
            }
            None
        }
        let s = find_string(tree.root_node()).expect("string must exist");
        assert!(php_string_callable_method_segment(s, source, LangId::Php).is_none());
    }

    /// 既知の identifier ノード種別が true を返すことを検証
    #[test]
    fn is_identifier_kind_matches() {
        assert!(is_identifier_kind("identifier"));
        assert!(is_identifier_kind("type_identifier"));
        assert!(is_identifier_kind("field_identifier"));
        assert!(is_identifier_kind("property_identifier"));
        assert!(is_identifier_kind("constant"));
        assert!(is_identifier_kind("name"));
        assert!(is_identifier_kind("word"));
    }

    /// 非 identifier ノード種別が false を返すことを検証
    #[test]
    fn is_identifier_kind_rejects_non_identifier() {
        assert!(!is_identifier_kind("function_definition"));
        assert!(!is_identifier_kind("block"));
        assert!(!is_identifier_kind("string"));
        assert!(!is_identifier_kind("comment"));
    }

    /// Rust の `pub fn` と struct field が同名のとき、フィールドアクセスや
    /// struct 宣言・初期化を関数参照として誤マッチしないことを検証
    /// (Issue: 2026-05-21-redact-impact-triage)
    #[test]
    fn find_references_rust_function_excludes_same_name_struct_fields() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.rs");
        std::fs::write(
            &a,
            r#"pub struct Cfg {
    pub redact: bool,
}

pub fn redact(input: &str) -> String {
    input.to_string()
}

fn build(flag: bool) -> Cfg {
    Cfg { redact: flag }
}

fn build_short() -> Cfg {
    let redact = true;
    Cfg { redact }
}

fn caller(cfg: &Cfg, data: &str) {
    if cfg.redact {
        let _ = redact(data);
    }
}
"#,
        )
        .unwrap();

        let refs = find_references("redact", dir.path(), Some("**/*.rs")).unwrap();
        let kinds: Vec<_> = refs.iter().map(|r| (r.line, r.kind)).collect();

        // 期待:
        // - L4 (`pub fn redact`) — Definition
        // - L18 (`let _ = redact(data)`) — Reference (関数呼び出し)
        // それ以外のフィールド系 (L1=struct field 宣言, L9=field_initializer,
        // L13=`let redact = true;` の binding ではなく、`Cfg { redact }` の shorthand,
        // L16=`cfg.redact` の field_expression) は含まれないこと
        assert!(
            kinds.iter().any(|(_, k)| *k == Some(RefKind::Definition)),
            "関数定義が含まれること: kinds={kinds:?}"
        );
        let refs_text: Vec<&str> = refs.iter().filter_map(|r| r.context.as_deref()).collect();
        // 関数呼び出しの行は含まれる
        assert!(
            refs_text.iter().any(|c| c.contains("redact(data)")),
            "関数呼び出し redact(data) は含まれるべき: {refs_text:?}"
        );
        // 純粋なフィールドアクセス / 宣言 / 初期化系は含まれない
        assert!(
            !refs_text.iter().any(|c| c.contains("pub redact: bool")),
            "struct field 宣言 'pub redact: bool' は除外されるべき: {refs_text:?}"
        );
        assert!(
            !refs_text.iter().any(|c| c.trim() == "redact: flag,"),
            "field_initializer 'redact: flag' は除外されるべき: {refs_text:?}"
        );
        assert!(
            !refs_text.iter().any(|c| c.contains("Cfg { redact }")),
            "shorthand 'Cfg {{ redact }}' は除外されるべき: {refs_text:?}"
        );
        assert!(
            !refs_text.iter().any(|c| c.contains("cfg.redact")),
            "field_expression 'cfg.redact' は除外されるべき: {refs_text:?}"
        );
    }

    /// destructuring pattern (`let Cfg { redact: v } = ...`) の field name も
    /// 関数参照として誤マッチしないことを検証
    /// (codex コミット前レビューでの追加指摘)
    #[test]
    fn find_references_rust_function_excludes_field_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.rs");
        std::fs::write(
            &a,
            r#"pub struct Cfg { pub redact: bool }
pub fn redact(input: &str) -> String { input.to_string() }
fn caller(cfg: Cfg, data: &str) {
    let Cfg { redact: value } = cfg;
    if value {
        let _ = redact(data);
    }
}
"#,
        )
        .unwrap();

        let refs = find_references("redact", dir.path(), Some("**/*.rs")).unwrap();
        let texts: Vec<&str> = refs.iter().filter_map(|r| r.context.as_deref()).collect();
        assert!(
            !texts
                .iter()
                .any(|c| c.contains("let Cfg { redact: value }")),
            "field_pattern の name 部は除外されるべき: {texts:?}"
        );
        assert!(
            texts.iter().any(|c| c.contains("redact(data)")),
            "関数呼び出しは残るべき: {texts:?}"
        );
    }

    /// メソッド呼び出し `obj.method()` の `method` 部は関数参照として残ることを検証
    #[test]
    fn find_references_rust_method_call_field_identifier_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.rs");
        std::fs::write(
            &a,
            r#"struct S;
impl S {
    fn run(&self) {}
}
fn caller(s: &S) {
    s.run();
}
"#,
        )
        .unwrap();

        let refs = find_references("run", dir.path(), Some("**/*.rs")).unwrap();
        let texts: Vec<&str> = refs.iter().filter_map(|r| r.context.as_deref()).collect();
        // 定義 (`fn run(&self) {}`) + メソッド呼び出し (`s.run();`) の 2 件
        assert!(
            texts.iter().any(|c| c.contains("s.run()")),
            "method call s.run() は関数参照として残るべき: {texts:?}"
        );
        assert!(
            texts.iter().any(|c| c.contains("fn run(&self)")),
            "定義 fn run は残るべき: {texts:?}"
        );
    }

    /// 単一 refs 検索が複数ファイルを横断し、定義を先頭に返すことを検証
    #[test]
    fn find_references_single_search_sorts_definition_first() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.rs");
        let b = dir.path().join("b.rs");
        std::fs::write(&a, "pub fn greet() {}\nfn main() { greet(); }\n").unwrap();
        std::fs::write(&b, "fn other() { crate::greet(); }\n").unwrap();

        let refs = find_references("greet", dir.path(), Some("**/*.rs")).unwrap();

        assert_eq!(refs.len(), 3);
        assert_eq!(refs[0].kind, Some(RefKind::Definition));
        assert_eq!(refs[0].line, 0);
        assert!(
            refs[1..]
                .iter()
                .all(|r| r.kind != Some(RefKind::Definition))
        );
    }

    /// 指定行のソースが正しく抽出され、前後の空白が除去されることを検証
    #[test]
    fn extract_line_context_basic() {
        let source = b"line0\n  line1  \nline2";
        let ctx = extract_line_context(source, 1);
        assert_eq!(ctx, "line1");
    }

    /// 範囲外の行に対して空文字を返すことを検証
    #[test]
    fn extract_line_context_out_of_range() {
        let source = b"only one line";
        let ctx = extract_line_context(source, 5);
        assert_eq!(ctx, "");
    }

    /// 改行なしで終わる最終行も正しく抽出できることを検証（memchr 版で新規テスト）
    #[test]
    fn extract_line_context_final_line_without_newline() {
        let source = b"first\nsecond";
        let ctx = extract_line_context(source, 1);
        assert_eq!(ctx, "second");
    }

    /// 巨大行を 256 バイト境界で切り詰めることを検証（minified コード防御）
    #[test]
    fn extract_line_context_truncates_long_line() {
        let long = "a".repeat(500);
        let source = format!("line0\n{long}");
        let ctx = extract_line_context(source.as_bytes(), 1);
        assert!(ctx.ends_with("..."), "256 バイト超は省略記号で終わるべき");
        assert!(ctx.len() <= 256 + 3, "256 バイト + '...' 以内に収まるべき");
    }

    /// UTF-8 境界で安全に切り詰められることを検証（マルチバイト文字の分割禁止）
    #[test]
    fn extract_line_context_utf8_boundary_safe() {
        // 「あ」は UTF-8 で 3 バイト。256B 境界を跨ぐ位置に配置する
        let mut long = "a".repeat(254);
        long.push_str("あいうえお");
        let source = format!("x\n{long}");
        let ctx = extract_line_context(source.as_bytes(), 1);
        // UTF-8 境界違反でパニックしないこと
        assert!(ctx.ends_with("..."));
        assert!(std::str::from_utf8(ctx.as_bytes()).is_ok());
    }

    /// lexer 経路 (extract_line_context_bytes) も巨大行を 256 バイトで切り詰めることを検証。
    /// tree-sitter 非対応言語 (Xojo) の minified/生成行によるメモリ・出力爆発を防ぐ。
    /// 中間行（改行あり）と最終行（改行なし）の両経路を確認する。
    #[test]
    fn extract_line_context_bytes_truncates_long_line() {
        let long = "a".repeat(500);
        let source = format!("line0\n{long}\n{long}");
        let ctx_mid = extract_line_context_bytes(source.as_bytes(), 1);
        assert!(
            ctx_mid.ends_with("..."),
            "256 バイト超は省略記号で終わるべき"
        );
        assert!(
            ctx_mid.len() <= 256 + 3,
            "256 バイト + '...' 以内に収まるべき"
        );
        let ctx_last = extract_line_context_bytes(source.as_bytes(), 2);
        assert!(
            ctx_last.ends_with("..."),
            "最終行（改行なし）も切り詰めるべき"
        );
        assert!(ctx_last.len() <= 256 + 3);
        // 通常行（256 バイト以下）は切り詰めない
        let ctx0 = extract_line_context_bytes(source.as_bytes(), 0);
        assert_eq!(ctx0, "line0");
    }

    /// lexer 経路も UTF-8 境界で安全に切り詰めることを検証（マルチバイト文字の分割禁止）
    #[test]
    fn extract_line_context_bytes_utf8_boundary_safe() {
        let mut long = "a".repeat(254);
        long.push_str("あいうえお");
        let source = format!("x\n{long}");
        let ctx = extract_line_context_bytes(source.as_bytes(), 1);
        assert!(ctx.ends_with("..."));
        assert!(std::str::from_utf8(ctx.as_bytes()).is_ok());
    }

    /// `LineIndex::new` が空ソースでも line 0 を空文字として扱えることを検証。
    #[test]
    fn line_index_handles_empty_source() {
        let source: &[u8] = b"";
        let index = LineIndex::new(source);
        // 空ソースは line_starts = [0] のみで、line 0 の bounds は (0, 0)。
        assert_eq!(index.line_bounds(0, 0), Some((0, 0)));
        // 範囲外行 (>=1) は None。
        assert_eq!(index.line_bounds(0, 1), None);
    }

    /// `LineIndex` が末尾改行のあるソースで最終空行を追加で含むことを検証。
    /// `b"a\n"` は通常 1 行扱い (line 0 = "a") + line 1 = "" の空末尾行。
    #[test]
    fn line_index_trailing_newline_creates_empty_last_line() {
        let source: &[u8] = b"a\n";
        let index = LineIndex::new(source);
        // line 0: "a"
        let (s0, e0) = index.line_bounds(source.len(), 0).unwrap();
        assert_eq!(&source[s0..e0], b"a");
        // line 1: 末尾改行直後の空行 (start = 2, end = source.len() = 2)
        let (s1, e1) = index.line_bounds(source.len(), 1).unwrap();
        assert_eq!(s1, 2);
        assert_eq!(e1, 2);
        // line 2: 範囲外
        assert_eq!(index.line_bounds(source.len(), 2), None);
    }

    /// `LineIndex` で連続改行による空行が正しく検出されることを検証。
    /// `b"a\n\nb"` → line 0 "a", line 1 "", line 2 "b"。
    #[test]
    fn line_index_handles_blank_lines() {
        let source: &[u8] = b"a\n\nb";
        let index = LineIndex::new(source);
        let (s0, e0) = index.line_bounds(source.len(), 0).unwrap();
        assert_eq!(&source[s0..e0], b"a");
        // line 1 は空行 (start = 2, end = 2)
        let (s1, e1) = index.line_bounds(source.len(), 1).unwrap();
        assert_eq!(s1, 2);
        assert_eq!(e1, 2);
        // line 2: 改行なしで終わる最終行
        let (s2, e2) = index.line_bounds(source.len(), 2).unwrap();
        assert_eq!(&source[s2..e2], b"b");
        assert_eq!(index.line_bounds(source.len(), 3), None);
    }

    /// `LineIndex` が改行なしの単一行を最終行として扱えることを検証。
    #[test]
    fn line_index_no_trailing_newline_last_line() {
        let source: &[u8] = b"only";
        let index = LineIndex::new(source);
        let (start, end) = index.line_bounds(source.len(), 0).unwrap();
        assert_eq!(&source[start..end], b"only");
        assert_eq!(index.line_bounds(source.len(), 1), None);
    }

    /// `extract_line_context_indexed` が単発 `extract_line_context` と同じ結果を返すことを
    /// 検証 (LineIndex 経由でも従来挙動が維持される)。
    #[test]
    fn extract_line_context_indexed_matches_legacy_path() {
        let source = b"alpha\n  beta  \ngamma";
        let index = LineIndex::new(source);
        for row in 0..4 {
            assert_eq!(
                extract_line_context_indexed(source, &index, row),
                extract_line_context(source, row),
                "row={row}"
            );
        }
    }

    /// Rust の定義ノード種別に function_item と struct_item が含まれることを検証
    #[test]
    fn definition_node_kinds_rust() {
        let kinds = definition_node_kinds(LangId::Rust);
        assert!(kinds.contains(&"function_item"));
        assert!(kinds.contains(&"struct_item"));
        assert!(kinds.contains(&"enum_item"));
        assert!(kinds.contains(&"trait_item"));
    }

    /// Python の定義ノード種別に function_definition と class_definition が含まれることを検証
    #[test]
    fn definition_node_kinds_python() {
        let kinds = definition_node_kinds(LangId::Python);
        assert!(kinds.contains(&"function_definition"));
        assert!(kinds.contains(&"class_definition"));
    }

    /// `split_path_segments` が "::" 区切りの各セグメントとバイトオフセットを返すことを検証
    #[test]
    fn split_path_segments_basic() {
        assert_eq!(split_path_segments("foo"), vec![("foo", 0)]);
        assert_eq!(
            split_path_segments("Option::is_none"),
            vec![("Option", 0), ("is_none", 8)]
        );
        assert_eq!(
            split_path_segments("a::b::c"),
            vec![("a", 0), ("b", 3), ("c", 6)]
        );
        assert!(split_path_segments("").is_empty());
    }

    /// ヘルパー: Rust ソースを tree-sitter でパースしてツリーを返す
    fn parse_rust(source: &str) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .expect("load rust language");
        parser.parse(source, None).expect("parse rust source")
    }

    /// serde の serialize_with = "..." 内の関数名が参照として収集されることを検証
    #[test]
    fn rust_attr_string_ref_detected_for_serialize_with() {
        let source = r#"
fn serialize_jst() {}
struct Foo;
impl Foo {
    fn placeholder() {}
}
#[derive(Serialize)]
struct Bar {
    #[serde(serialize_with = "serialize_jst")]
    time: i64,
}
"#;
        let tree = parse_rust(source);
        let defs = definition_node_kinds(LangId::Rust);
        let mut refs = Vec::new();
        let line_index = LineIndex::new(source.as_bytes());
        collect_identifier_refs(
            tree.root_node(),
            source.as_bytes(),
            &line_index,
            "serialize_jst",
            "test.rs",
            defs,
            LangId::Rust,
            &mut refs,
        );

        // 定義 1 件 + 属性文字列内参照 1 件
        let def_cnt = refs
            .iter()
            .filter(|r| matches!(r.kind, Some(RefKind::Definition)))
            .count();
        let ref_cnt = refs
            .iter()
            .filter(|r| matches!(r.kind, Some(RefKind::Reference)))
            .count();
        assert_eq!(def_cnt, 1, "definition should be captured");
        assert_eq!(ref_cnt, 1, "serde attribute string ref should be captured");
    }

    /// 属性文字列参照が非 Definition としてカウントされ、dead-code 判定に反映されることを検証
    #[test]
    fn rust_attr_string_ref_counted_as_non_definition() {
        use std::borrow::Cow;
        use std::collections::HashMap;

        let source = r#"
fn serialize_jst() {}
#[derive(Serialize)]
struct Bar {
    #[serde(serialize_with = "serialize_jst")]
    time: i64,
}
"#;
        let tree = parse_rust(source);
        let defs = definition_node_kinds(LangId::Rust);
        let mut name_to_ix: HashMap<Cow<'_, str>, Vec<usize>> = HashMap::new();
        name_to_ix.insert(Cow::Borrowed("serialize_jst"), vec![0]);
        let mut counts = vec![0usize];
        count_identifier_refs(
            tree.root_node(),
            source.as_bytes(),
            &name_to_ix,
            defs,
            LangId::Rust,
            &mut counts,
        );
        assert_eq!(counts[0], 1, "attribute string ref must lift dead-code");
    }

    /// `Option::is_none` のようなパス文字列では最終セグメントもカウントされることを検証
    #[test]
    fn rust_attr_string_ref_path_segments() {
        let source = r#"
#[derive(Serialize)]
struct Bar {
    #[serde(skip_serializing_if = "Option::is_none")]
    inner: Option<i64>,
}
"#;
        let tree = parse_rust(source);
        let defs = definition_node_kinds(LangId::Rust);
        let mut refs = Vec::new();
        let line_index = LineIndex::new(source.as_bytes());
        collect_identifier_refs(
            tree.root_node(),
            source.as_bytes(),
            &line_index,
            "is_none",
            "test.rs",
            defs,
            LangId::Rust,
            &mut refs,
        );
        assert_eq!(
            refs.len(),
            1,
            "path tail segment should be matched as reference"
        );
    }

    /// 対象外キー (例: rename) の文字列値は参照として扱わないことを検証
    #[test]
    fn rust_attr_string_ref_ignores_non_ref_keys() {
        let source = r#"
#[derive(Serialize)]
struct Bar {
    #[serde(rename = "created_at")]
    time: i64,
}
"#;
        let tree = parse_rust(source);
        let defs = definition_node_kinds(LangId::Rust);
        let mut refs = Vec::new();
        let line_index = LineIndex::new(source.as_bytes());
        collect_identifier_refs(
            tree.root_node(),
            source.as_bytes(),
            &line_index,
            "created_at",
            "test.rs",
            defs,
            LangId::Rust,
            &mut refs,
        );
        assert!(
            refs.is_empty(),
            "rename is not a reference key and must not match"
        );
    }

    /// 非 Rust 言語では属性文字列ヒューリスティックが動作しないことを検証
    #[test]
    fn rust_attr_helper_is_noop_for_other_languages() {
        // Python AST 上に string_content が登場しても反応しない
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .expect("load python language");
        let source = "x = \"serialize_jst\"\n";
        let tree = parser.parse(source, None).unwrap();
        let segs = collect_all_attr_segments(tree.root_node(), source.as_bytes(), LangId::Python);
        assert!(segs.is_empty());
    }

    /// ヘルパー: 木全体で rust_attr_string_ref_segments が拾うセグメントを再帰収集
    fn collect_all_attr_segments<'a>(
        node: Node<'a>,
        source: &'a [u8],
        lang_id: LangId,
    ) -> Vec<(String, usize, usize)> {
        let mut out: Vec<(String, usize, usize)> =
            rust_attr_string_ref_segments(node, source, lang_id)
                .into_iter()
                .map(|(s, r, c)| (s.to_string(), r, c))
                .collect();
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            out.extend(collect_all_attr_segments(child, source, lang_id));
        }
        out
    }

    #[test]
    fn is_generated_file_detects_laravel_ide_helper() {
        let path = Path::new("Laravel/www/_ide_helper.php");
        assert!(super::is_generated_file(path));
        let path = Path::new("Laravel/www/_ide_helper_models.php");
        assert!(super::is_generated_file(path));
        let path = Path::new("Laravel/www/_lighthouse_ide_helper.php");
        assert!(super::is_generated_file(path));
    }

    #[test]
    fn is_generated_file_passes_vendor_dir() {
        // vendor/node_modules はここではなく呼び出し側 (dead-code の
        // --include-vendor opt-in 等) で制御するため、is_generated_file は通す。
        let path = Path::new("Laravel/www/vendor/composer/Foo.php");
        assert!(!super::is_generated_file(path));
        let path = Path::new("Angular/www/node_modules/lib/index.ts");
        assert!(!super::is_generated_file(path));
    }

    #[test]
    fn is_generated_file_detects_minified_assets() {
        assert!(super::is_generated_file(Path::new("public/app.min.js")));
        assert!(super::is_generated_file(Path::new("public/app.min.css")));
        assert!(super::is_generated_file(Path::new("public/app.bundle.js")));
    }

    #[test]
    fn is_generated_file_passes_normal_source() {
        // 通常のソースファイルは false
        let path = Path::new("src/app/services/log.service.ts");
        assert!(!super::is_generated_file(path));
        let path = Path::new("Laravel/www/app/Http/Controllers/UserController.php");
        assert!(!super::is_generated_file(path));
    }

    #[test]
    fn is_generated_file_detects_at_generated_marker() {
        // ファイル先頭マーカー判定を確認
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("foo.php");
        std::fs::write(&path, "<?php\n// @generated by some-tool\nclass X {}\n").expect("write");
        assert!(super::is_generated_file(&path));

        let path2 = dir.path().join("bar.php");
        std::fs::write(&path2, "<?php\n// hand-written\nclass Y {}\n").expect("write");
        assert!(!super::is_generated_file(&path2));
    }

    #[test]
    fn is_generated_file_detects_do_not_edit_marker() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("gen.go");
        std::fs::write(
            &path,
            "// Code generated by protoc-gen-go. DO NOT EDIT.\npackage foo\n",
        )
        .expect("write");
        assert!(super::is_generated_file(&path));
    }

    #[test]
    fn collect_files_handles_generated_exclusion() {
        // 並列テストで env を共有するため 1 つのテストに集約してシリアル化する。
        // (1) デフォルトで _ide_helper.php が除外される
        // (2) ASTRO_SIGHT_NO_GENERATED_EXCLUSION=1 で opt-out できる
        let prev = std::env::var("ASTRO_SIGHT_NO_GENERATED_EXCLUSION").ok();
        // 念のため事前に unset して default 動作を保証する
        // SAFETY: Rust 2024 Edition では set_var/remove_var は unsafe 扱い。
        // env を読む他テストは存在しない (検索済み) ため本テスト内のみ操作する。
        unsafe {
            std::env::remove_var("ASTRO_SIGHT_NO_GENERATED_EXCLUSION");
        }

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("_ide_helper.php"), "<?php class X {}\n").expect("write");
        std::fs::write(dir.path().join("real.php"), "<?php class Real {}\n").expect("write");

        // (1) default: _ide_helper.php は除外、real.php は含まれる
        let files = super::collect_files(dir.path(), None).expect("collect");
        let names: Vec<_> = files
            .iter()
            .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
            .collect();
        let default_real = names.contains(&"real.php");
        let default_ide = names.contains(&"_ide_helper.php");

        // (2) opt-out: env="1" で _ide_helper.php も含まれる
        // SAFETY: Rust 2024 で unsafe。テスト終了時に restore する。
        unsafe {
            std::env::set_var("ASTRO_SIGHT_NO_GENERATED_EXCLUSION", "1");
        }
        let files = super::collect_files(dir.path(), None).expect("collect");
        let optout_ide = files
            .iter()
            .any(|p| p.file_name().and_then(|n| n.to_str()) == Some("_ide_helper.php"));

        // restore
        // SAFETY: テスト終了処理。
        unsafe {
            match prev {
                Some(v) => std::env::set_var("ASTRO_SIGHT_NO_GENERATED_EXCLUSION", v),
                None => std::env::remove_var("ASTRO_SIGHT_NO_GENERATED_EXCLUSION"),
            }
        }

        assert!(
            default_real,
            "通常ファイルは default で含まれるべき。got names: {names:?}"
        );
        assert!(
            !default_ide,
            "_ide_helper.php は default で除外されるべき。got names: {names:?}"
        );
        assert!(optout_ide, "opt-out 時は _ide_helper.php も含まれるべき");
    }

    /// bash の `trap '<handler>' SIG` 内の関数参照が `count_identifier_refs` で
    /// 非 Definition としてカウントされ、dead-code 判定で生存扱いになることを検証する。
    /// 旧実装では trap handler 内は文字列扱いで参照ゼロとなり、`cleanup_signal` のような
    /// シグナルハンドラが false-positive で dead として列挙される回帰があった。
    #[test]
    fn bash_trap_handler_counts_as_non_definition_ref() {
        use std::borrow::Cow;
        use std::collections::HashMap;

        let source = "cleanup_signal() {\n    exit 1\n}\ntrap 'cleanup_signal 130' INT\ntrap \"cleanup_signal 143\" TERM\n";
        let tree = parser::parse_source(source.as_bytes(), LangId::Bash).expect("parse");
        let defs = definition_node_kinds(LangId::Bash);
        let mut name_to_ix: HashMap<Cow<'_, str>, Vec<usize>> = HashMap::new();
        name_to_ix.insert(Cow::Borrowed("cleanup_signal"), vec![0]);
        let mut counts = vec![0usize];
        count_identifier_refs(
            tree.root_node(),
            source.as_bytes(),
            &name_to_ix,
            defs,
            LangId::Bash,
            &mut counts,
        );
        assert_eq!(
            counts[0], 2,
            "bash trap handler 内の関数参照は 2 件カウントされるべき (single+double quoted)"
        );
    }

    /// `find_references` (CLI の `astro-sight refs --name`) が bash trap handler
    /// 内の関数参照を返すこと。Issue #5 で報告された再現を回帰テスト化したもの。
    #[test]
    fn bash_trap_handler_resolved_in_find_references() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("update_server.sh");
        std::fs::write(
            &script,
            "#!/bin/bash\ncleanup_signal() {\n    local sig_exit=$1\n    exit \"${sig_exit}\"\n}\ntrap 'cleanup_signal 130' INT\ntrap 'cleanup_signal 143' TERM\n",
        )
        .unwrap();

        let refs = find_references("cleanup_signal", dir.path(), None).unwrap();
        // 期待: 定義 1 件 + trap 経由参照 2 件
        let defs: Vec<_> = refs
            .iter()
            .filter(|r| r.kind == Some(RefKind::Definition))
            .collect();
        let non_defs: Vec<_> = refs
            .iter()
            .filter(|r| r.kind != Some(RefKind::Definition))
            .collect();
        assert_eq!(defs.len(), 1, "definition should be 1, got refs={refs:?}");
        assert_eq!(
            non_defs.len(),
            2,
            "trap handler refs should be 2, got refs={refs:?}"
        );
    }

    /// 引用なし `trap func SIG` (`word` ノード) は通常の identifier 走査で拾われるため、
    /// bash_trap_handler_ref_segments 側では二重カウントしないことを検証する。
    #[test]
    fn bash_trap_unquoted_word_not_double_counted() {
        use std::borrow::Cow;
        use std::collections::HashMap;

        let source = "cleanup() { exit 1; }\ntrap cleanup INT\n";
        let tree = parser::parse_source(source.as_bytes(), LangId::Bash).expect("parse");
        let defs = definition_node_kinds(LangId::Bash);
        let mut name_to_ix: HashMap<Cow<'_, str>, Vec<usize>> = HashMap::new();
        name_to_ix.insert(Cow::Borrowed("cleanup"), vec![0]);
        let mut counts = vec![0usize];
        count_identifier_refs(
            tree.root_node(),
            source.as_bytes(),
            &name_to_ix,
            defs,
            LangId::Bash,
            &mut counts,
        );
        // 引用なし `trap cleanup INT` は通常の word 走査で 1 件として拾われる。
        // bash_trap_handler_ref_segments は raw_string/string のみ対象なので加算しない。
        assert_eq!(counts[0], 1, "unquoted word must not be double-counted");
    }

    /// PHP のメソッド呼び出しは case-insensitive に解決される。
    /// 定義 `isFooBar` と呼び出し `isFoobar` (case 違い) は同一メソッドに解決される。
    #[test]
    fn find_references_php_method_call_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Vo.php"),
            "<?php\nclass Vo {\n    public function isFooBar(): bool { return true; }\n}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("Caller.php"),
            "<?php\nclass Caller {\n    public function check(Vo $vo): bool { return $vo->isFoobar(); }\n}\n",
        )
        .unwrap();

        let refs = find_references("isFooBar", dir.path(), None).unwrap();
        let defs = refs
            .iter()
            .filter(|r| r.kind == Some(RefKind::Definition))
            .count();
        let non_defs = refs
            .iter()
            .filter(|r| r.kind != Some(RefKind::Definition))
            .count();
        assert_eq!(defs, 1, "definition should resolve, got refs={refs:?}");
        assert_eq!(
            non_defs, 1,
            "case-different method call must resolve as reference, got refs={refs:?}"
        );
    }

    /// PHP の静的メソッド呼び出し (`Foo::bar()`) も case-insensitive に解決される。
    #[test]
    fn find_references_php_static_method_call_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.php"),
            "<?php\nclass Svc {\n    public static function doIt(): void {}\n    public function run(): void { Svc::DOIT(); }\n}\n",
        )
        .unwrap();

        let refs = find_references("doIt", dir.path(), None).unwrap();
        let non_defs = refs
            .iter()
            .filter(|r| r.kind != Some(RefKind::Definition))
            .count();
        assert_eq!(
            non_defs, 1,
            "case-different static call must resolve, got refs={refs:?}"
        );
    }

    /// PHP のクラス名は case-insensitive。`new FOO()` / `new Foo()` が定義 `class Foo` に解決される。
    #[test]
    fn find_references_php_class_name_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Foo.php"),
            "<?php\nclass Foo {\n    public function go(): void {}\n}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("use.php"),
            "<?php\nfunction make() {\n    $a = new FOO();\n    $b = new Foo();\n    return $a;\n}\n",
        )
        .unwrap();

        let refs = find_references("Foo", dir.path(), None).unwrap();
        let non_defs = refs
            .iter()
            .filter(|r| r.kind != Some(RefKind::Definition))
            .count();
        assert_eq!(
            non_defs, 2,
            "class name `new FOO`/`new Foo` must resolve case-insensitively, got refs={refs:?}"
        );
    }

    /// PHP のプロパティ名は case-sensitive。大小違いの検索は member_access に一致しない。
    #[test]
    fn find_references_php_property_access_is_case_sensitive() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.php"),
            "<?php\nclass C {\n    public int $myProp = 0;\n    public function f(): int { return $this->myProp; }\n}\n",
        )
        .unwrap();

        // case-fold していれば "MYPROP" が "myProp" に誤マッチするが、プロパティは case-sensitive。
        let refs = find_references("MYPROP", dir.path(), None).unwrap();
        assert!(
            refs.is_empty(),
            "property access is case-sensitive; uppercase search must not match, got refs={refs:?}"
        );
    }

    /// PHP のクラス定数は case-sensitive。大小違いの検索は class_constant_access に一致しない。
    #[test]
    fn find_references_php_class_constant_is_case_sensitive() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.php"),
            "<?php\nclass C {\n    const MyConst = 1;\n    public function f(): int { return self::MyConst; }\n}\n",
        )
        .unwrap();

        let refs = find_references("MYCONST", dir.path(), None).unwrap();
        assert!(
            refs.is_empty(),
            "class constant is case-sensitive; uppercase search must not match, got refs={refs:?}"
        );
    }

    /// バッチ参照検索 (dead-code / api が使う経路) でも PHP メソッドの case 違いが解決される。
    #[test]
    fn find_references_batch_php_method_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Vo.php"),
            "<?php\nclass Vo {\n    public function isFooBar(): bool { return true; }\n}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("Caller.php"),
            "<?php\nclass Caller {\n    public function check(Vo $vo): bool { return $vo->isFoobar(); }\n}\n",
        )
        .unwrap();

        let map = find_references_batch(&["isFooBar".to_string()], dir.path(), None).unwrap();
        let refs = map.get("isFooBar").cloned().unwrap_or_default();
        let non_defs = refs
            .iter()
            .filter(|r| r.kind != Some(RefKind::Definition))
            .count();
        assert_eq!(
            non_defs, 1,
            "batch path must resolve case-different call, got refs={refs:?}"
        );
    }

    /// PHP の名前空間付きクラス参照 (`use App\Foo` / 型ヒント / `new \App\Foo()`) も
    /// case-insensitive に解決される (qualified_name の末尾 name)。
    #[test]
    fn find_references_php_namespaced_class_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Foo.php"),
            "<?php\nnamespace App\\Repo;\nclass UserRepository {\n    public function go(): void {}\n}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("use.php"),
            "<?php\nuse App\\Repo\\USERREPOSITORY;\nfunction make(\\App\\Repo\\Userrepository $r) {\n    return new \\App\\Repo\\userRepository();\n}\n",
        )
        .unwrap();

        // 参照: use の USERREPOSITORY / 型ヒント Userrepository / new userRepository (全て case 違い)。
        let refs = find_references("UserRepository", dir.path(), None).unwrap();
        let non_defs = refs
            .iter()
            .filter(|r| r.kind != Some(RefKind::Definition))
            .count();
        assert!(
            non_defs >= 3,
            "namespaced class refs must resolve case-insensitively, got refs={refs:?}"
        );
    }

    /// PHP の trait adaptation (`insteadof` / `as`) のメソッド名も case-insensitive に解決される。
    #[test]
    fn find_references_php_trait_adaptation_method_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("t.php"),
            "<?php\ntrait A {\n    public function work(): void {}\n}\ntrait B {\n    public function work(): void {}\n}\nclass C {\n    use A, B {\n        B::WORK insteadof A;\n        A::Work as legacyWork;\n    }\n}\n",
        )
        .unwrap();

        // B::WORK (insteadof) と A::Work (as) は trait メソッド work の case 違い参照。
        let refs = find_references("work", dir.path(), None).unwrap();
        let non_defs = refs
            .iter()
            .filter(|r| r.kind != Some(RefKind::Definition))
            .count();
        assert!(
            non_defs >= 2,
            "trait adaptation method refs must resolve case-insensitively, got refs={refs:?}"
        );
    }

    /// PHP の `use const` (定数 import) は case-sensitive、`use` (クラス import) は case-insensitive。
    #[test]
    fn find_references_php_use_const_is_case_sensitive() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.php"),
            "<?php\nnamespace App;\nuse const App\\Config\\MAX_SIZE;\nuse App\\Repo\\UserRepo;\n",
        )
        .unwrap();

        // use const の定数名は case-sensitive (大小違いは一致しない)。
        assert!(
            find_references("max_size", dir.path(), None)
                .unwrap()
                .is_empty(),
            "use const must be case-sensitive"
        );
        // 対照: use (クラス import) は case-insensitive。
        assert!(
            !find_references("USERREPO", dir.path(), None)
                .unwrap()
                .is_empty(),
            "use class import must be case-insensitive"
        );
    }

    /// PHP の group use 内でも const は case-sensitive、クラス / 関数は case-insensitive。
    #[test]
    fn find_references_php_group_use_const_is_case_sensitive() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.php"),
            "<?php\nnamespace App;\nuse App\\{ Foo, const MAX_LEN, function helper };\n",
        )
        .unwrap();

        assert!(
            find_references("max_len", dir.path(), None)
                .unwrap()
                .is_empty(),
            "group use const must be case-sensitive"
        );
        assert!(
            !find_references("FOO", dir.path(), None).unwrap().is_empty(),
            "group use class must be case-insensitive"
        );
        assert!(
            !find_references("HELPER", dir.path(), None)
                .unwrap()
                .is_empty(),
            "group use function must be case-insensitive"
        );
    }
}
