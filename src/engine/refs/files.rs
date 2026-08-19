//! 参照検索・dead-code 判定の走査対象ファイル収集。
//!
//! `.gitignore` 尊重 + ディレクトリ名 / ネガティブ glob による除外 + 生成物
//! (minified / `@generated` マーカー / IDE ヘルパー) の除外を担う。

use anyhow::Result;
use std::path::Path;

use crate::language::LangId;
use crate::models::skip::SkippedFiles;

const SKIPPED_PATHS_CAP: usize = 50;

/// Generated-file handling for a directory scan.
#[derive(Debug, Clone, Copy, Default)]
pub struct FileScanOptions {
    /// Include files that would otherwise be identified as generated.
    pub include_generated: bool,
}

/// Files selected for parsing plus generated files intentionally omitted.
#[derive(Debug, Default)]
pub struct FileCollection {
    pub files: Vec<std::path::PathBuf>,
    skipped_generated: Vec<std::path::PathBuf>,
}

impl FileCollection {
    /// Build bounded, deterministic metadata for machine-readable command output.
    pub fn skipped(&self, dir: &Path) -> Option<SkippedFiles> {
        if self.skipped_generated.is_empty() {
            return None;
        }
        let generated = self.skipped_generated.len();
        let mut paths: Vec<String> = self
            .skipped_generated
            .iter()
            .map(|path| {
                path.strip_prefix(dir)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        paths.sort();
        paths.truncate(SKIPPED_PATHS_CAP);
        Some(SkippedFiles {
            generated,
            truncated: generated > paths.len(),
            paths,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateDecision {
    Keep,
    SkipGenerated,
    Ignore,
}

/// ignore クレートでファイルを収集する（.gitignore 対応）。
pub fn collect_files(dir: &Path, glob_pattern: Option<&str>) -> Result<Vec<std::path::PathBuf>> {
    Ok(collect_files_scan(dir, glob_pattern, FileScanOptions::default())?.files)
}

/// Collect parseable files and report generated files omitted from the scan.
pub fn collect_files_scan(
    dir: &Path,
    glob_pattern: Option<&str>,
    options: FileScanOptions,
) -> Result<FileCollection> {
    collect_files_scan_with_excludes(dir, glob_pattern, &[], &[], options)
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
    Ok(collect_files_scan_with_excludes(
        dir,
        glob_pattern,
        excluded_dir_names,
        excluded_globs,
        FileScanOptions::default(),
    )?
    .files)
}

fn collect_files_scan_with_excludes(
    dir: &Path,
    glob_pattern: Option<&str>,
    excluded_dir_names: &[&str],
    excluded_globs: &[&str],
    options: FileScanOptions,
) -> Result<FileCollection> {
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

    // walk 自体は逐次で候補パスだけを集め、ファイル内容を読む判定 (generated マーカー /
    // shebang) は per-file 並列ステージへ後送する。旧実装は walk ループ内で全候補を
    // 逐次 open + 先頭 4KB read しており、大規模リポでは open の直列待ちが
    // ボトルネックだった (profiler で __open が上位)。
    let mut candidates = Vec::new();
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
        candidates.push(path);
    }

    // 判定は「名前ベース generated → 言語判定 → 内容ベース generated / shebang」の順。
    // 旧実装 (内容ベース generated → 言語判定) と最終集合は同一で、言語判定を先に
    // 通すことで parse 対象になり得ないファイル (.png 等) の open を丸ごと省く。
    // par_iter + collect は入力順を保つため、出力順は逐次実装と一致する。
    use rayon::prelude::*;
    let exclude_generated = !options.include_generated
        && !is_generated_exclusion_disabled()
        && !glob_names_explicit_file(glob_pattern);
    let decisions: Vec<(std::path::PathBuf, CandidateDecision)> = candidates
        .into_par_iter()
        .map(|path| {
            let decision = classify_candidate(&path, exclude_generated);
            (path, decision)
        })
        .collect();

    let mut files = Vec::new();
    let mut skipped_generated = Vec::new();
    for (path, decision) in decisions {
        match decision {
            CandidateDecision::Keep => files.push(path),
            CandidateDecision::SkipGenerated => skipped_generated.push(path),
            CandidateDecision::Ignore => {}
        }
    }

    Ok(FileCollection {
        files,
        skipped_generated,
    })
}

/// 収集候補 1 ファイルを最終集合に残すかの判定。
///
/// - ベンダー / IDE 補助 / `@generated` マーカー付きファイルはノイズ源になるため
///   refs / impact / dead-code から除外する (`ASTRO_SIGHT_NO_GENERATED_EXCLUSION=1` で抑止可)。
/// - パース可能なファイルのみ対象。拡張子なしの実行スクリプト (例: `bin/install`) は
///   shebang から言語推定する (CLI ツール / ビルドスクリプトで shebang 命名は一般的)。
/// - 拡張子なしファイルは先頭 4KB を 1 回だけ読み、generated マーカー判定と shebang
///   判定を同じバッファで行う (旧実装は 2 回 open していた)。
fn classify_candidate(path: &Path, exclude_generated: bool) -> CandidateDecision {
    if exclude_generated && is_generated_by_name(path) {
        return CandidateDecision::SkipGenerated;
    }
    if LangId::from_path(camino::Utf8Path::new(path.to_str().unwrap_or(""))).is_ok() {
        return if exclude_generated && has_generated_marker(path) {
            CandidateDecision::SkipGenerated
        } else {
            CandidateDecision::Keep
        };
    }
    if path.extension().is_some() {
        return CandidateDecision::Ignore;
    }
    let Some(head) = read_head_4k(path) else {
        return CandidateDecision::Ignore;
    };
    if exclude_generated && head_has_generated_marker(&head) {
        return CandidateDecision::SkipGenerated;
    }
    if detect_lang_from_shebang_head(&head).is_some() {
        CandidateDecision::Keep
    } else {
        CandidateDecision::Ignore
    }
}

/// `ASTRO_SIGHT_NO_GENERATED_EXCLUSION=1` のときだけ generated ファイル除外を抑止する。
fn is_generated_exclusion_disabled() -> bool {
    std::env::var("ASTRO_SIGHT_NO_GENERATED_EXCLUSION")
        .ok()
        .filter(|v| !v.is_empty() && v != "0")
        .is_some()
}

/// A glob explicitly names files when its final path segment has no glob meta.
/// `**/parser.c` is explicit; `**/*.c` remains a directory-wide filtered scan.
fn glob_names_explicit_file(glob_pattern: Option<&str>) -> bool {
    let Some(pattern) = glob_pattern else {
        return false;
    };
    let basename = pattern.rsplit('/').next().unwrap_or(pattern);
    !basename.is_empty()
        && !basename
            .chars()
            .any(|ch| matches!(ch, '*' | '?' | '[' | ']' | '{' | '}'))
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
///
/// 本体経路は `keep_candidate` が名前判定と内容判定を分けて呼ぶため、この合成形は
/// 判定仕様を固定するテスト専用ヘルパーとして残す。
#[cfg(test)]
pub(crate) fn is_generated_file(path: &Path) -> bool {
    is_generated_by_name(path) || has_generated_marker(path)
}

/// ファイル名パターンだけで判定できる generated 判定 (I/O なし)。
fn is_generated_by_name(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if name.starts_with("_ide_helper") && name.ends_with(".php") {
        return true;
    }
    if name == "_lighthouse_ide_helper.php" {
        return true;
    }
    name.ends_with(".min.js") || name.ends_with(".min.css") || name.ends_with(".bundle.js")
}

/// ファイル先頭最大 4KB を読む。読めない場合は None (パーミッション欠落等)。
fn read_head_4k(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut buf = [0u8; 4096];
    let n = file.read(&mut buf).ok()?;
    Some(buf[..n].to_vec())
}

/// ファイル先頭 4KB に generated マーカーが含まれるかを判定する。
///
/// I/O コスト軽減のため最大 4KB のみ読む。読めない場合は false を返す
/// (パーミッション欠落等で誤って除外しない)。
fn has_generated_marker(path: &Path) -> bool {
    read_head_4k(path).is_some_and(|head| head_has_generated_marker(&head))
}

/// 読み込み済み先頭バッファに対する generated マーカー判定。マーカー文字列は UTF-8
/// 妥当性を問わずバイト列としてマッチさせる (memchr::memmem)。
fn head_has_generated_marker(head: &[u8]) -> bool {
    crate::engine::generated::head_declares_generated(head)
}

/// 読み込み済み先頭バッファの shebang 行から言語を判定する。
///
/// 最初の 2 byte が `#!` でなければ即時 None。実行ビット等は判定せず、
/// shebang の有無だけで言語推定可能かを決める。
fn detect_lang_from_shebang_head(head: &[u8]) -> Option<LangId> {
    if head.len() < 2 || &head[..2] != b"#!" {
        return None;
    }
    let line_end = head.iter().position(|&b| b == b'\n').unwrap_or(head.len());
    let first_line = std::str::from_utf8(&head[..line_end]).ok()?;
    LangId::from_shebang(first_line)
}

/// パスのいずれかの中間ディレクトリ名が除外対象と完全一致するかを判定する。
fn path_has_excluded_segment(path: &Path, excluded: &[&str]) -> bool {
    path.components().any(|c| match c.as_os_str().to_str() {
        Some(name) => excluded.contains(&name),
        None => false,
    })
}

/// workspace walk の結果 `files` に、diff 由来などの明示ファイルを canonical path で
/// 合流させる (workspace 外・解決不能・重複は除外)。hidden ディレクトリ配下でも候補に
/// なったファイル自身の参照を取りこぼさないための共通ヘルパー
/// (count 経路と member liveness 経路で走査集合を一致させる)。
pub fn merge_extra_files(
    files: &mut Vec<std::path::PathBuf>,
    canonical_dir: &Path,
    extra_files: &[std::path::PathBuf],
) {
    if extra_files.is_empty() {
        return;
    }
    let mut seen: std::collections::HashSet<std::path::PathBuf> = files.iter().cloned().collect();
    for extra in extra_files {
        let candidate = if extra.is_absolute() {
            extra.clone()
        } else {
            canonical_dir.join(extra)
        };
        let Ok(canonical) = std::fs::canonicalize(candidate) else {
            continue;
        };
        if canonical.starts_with(canonical_dir) && seen.insert(canonical.clone()) {
            files.push(canonical);
        }
    }
}
