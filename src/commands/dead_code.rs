use anyhow::Result;
use std::collections::HashSet;
use tracing::info;

use crate::engine::parser;
use crate::error::{AstroError, ErrorCode};
use crate::git_support::normalize_workspace_separators;
use crate::models::dead_code::DeadCodeResult;
use crate::models::review::DeadSymbol;

#[cfg(test)]
use super::api_changes::extract_exported_symbols_from_file_inner;
use super::api_changes::{ExportedSymbols, bare_name, filter_exported_symbols};
use super::dead_code_default_liveness::DefaultExportLiveness;
use super::dead_code_member_liveness::{
    JsTsMemberLiveness, MemberStatus, PhpMemberLiveness, RubyConstructorLiveness,
    RustMemberLiveness,
};
use super::dead_code_swift_protocol::SwiftProtocolLiveness;
use super::git_input::{DiffSourceResolution, resolve_diff_source};
use crate::config::SwiftProtocolMethods;
use crate::output::{OutputOptions, serialize_cli_document};

/// dead-code 検出の結果一式。
#[derive(Debug, Default)]
pub(crate) struct DeadCodeDetection {
    pub(crate) dead: Vec<DeadSymbol>,
    pub(crate) test_only: Vec<DeadSymbol>,
    /// 生成物として dead 候補から外したファイル (`dir` 相対、`/` 区切り)。
    /// 参照の集計には含めている。
    pub(crate) generated_candidates_skipped: Vec<String>,
    /// 参照集計の走査で見つかった「解析できないソース」の申告。候補が 0 件で参照集計を
    /// 行わなかった場合は空 (dead と断定したものが無いので誤検出の余地も無い)。
    pub(crate) unanalyzable: Vec<crate::models::truncation::TruncationInfo>,
    /// 参照を数えられなかったファイル (読み込み・構文解析の失敗) の申告。
    /// 1 件でもあれば、件数は入力の一部しか数えていない。
    pub(crate) uncounted: Vec<crate::models::truncation::TruncationInfo>,
    /// 参照の件数だけなら dead / test_only だが、数えられなかったファイルから参照されて
    /// いるかもしれないため断定を保留した候補。
    pub(crate) unverified: Vec<DeadSymbol>,
}

impl DeadCodeDetection {
    /// dead を 1 件も断定しなかった結果。候補から外した生成物の申告だけは残す。
    fn without_dead(generated_candidates_skipped: Vec<String>) -> Self {
        Self {
            generated_candidates_skipped,
            ..Self::default()
        }
    }
}

/// dead-code 検出本体 (既定の生成物の扱い)。`(dead_symbols, test_only_symbols)` を返す
/// テスト用の簡易版 (申告と保留した候補は捨てる。製品の経路は `detect_dead_symbols` を使う)。
#[cfg(test)]
pub(crate) fn detect_dead_symbols_from_files(
    dir: &str,
    files: &[std::path::PathBuf],
) -> (Vec<DeadSymbol>, Vec<DeadSymbol>) {
    let detection = detect_dead_symbols(dir, files, false);
    (detection.dead, detection.test_only)
}

/// dead-code 検出本体。候補収集 → 参照走査集合の収集 → 名前インデックス構築 →
/// 参照カウント → アセット参照収集 → 分類の段階パイプライン。
///
/// `include_generated` は生成物を dead の**候補**に含めるかだけを決める。参照の集計には
/// 生成物を常に含める (`FileScanOptions::DEAD_CODE_REFERENCES`)。
#[cfg(test)]
pub(crate) fn detect_dead_symbols(
    dir: &str,
    files: &[std::path::PathBuf],
    include_generated: bool,
) -> DeadCodeDetection {
    detect_dead_symbols_with_swift_protocols(
        dir,
        files,
        include_generated,
        &SwiftProtocolMethods::default(),
    )
}

pub(crate) fn detect_dead_symbols_with_swift_protocols(
    dir: &str,
    files: &[std::path::PathBuf],
    include_generated: bool,
    swift_protocols: &SwiftProtocolMethods,
) -> DeadCodeDetection {
    let canonical_dir = match std::fs::canonicalize(dir) {
        Ok(d) => d,
        Err(_) => return DeadCodeDetection::default(),
    };

    // case-insensitive 言語 (Xojo 等) のみで構成された files では dead-code 検出を
    // skip する。
    //
    // v26.5 まで: CI 言語 (Xojo) は tree-sitter parse が OOM する問題で diff 全体を skip。
    // v26.6 以降: tree-sitter-xojo を削除し lexer-only に移行。dead-code は lexer 経由で
    // 動作するため CI skip 機構は不要。`ASTRO_SIGHT_FORCE_CI_LANG_DEAD_CODE` は deprecate
    // (no-op、警告も出さない)。

    let mut candidates =
        collect_dead_code_candidates(dir, &canonical_dir, files, include_generated);
    let generated_candidates_skipped = std::mem::take(&mut candidates.generated_skipped);
    if candidates.all_syms.is_empty() {
        return DeadCodeDetection::without_dead(generated_candidates_skipped);
    }

    // 参照は候補の絞り込み (`--glob` / 既定のディレクトリ除外 / `--git`) と無関係に
    // ディレクトリ全体から来るので、走査集合も全体にする。解析できないソースの申告も
    // 同じ集合から出し、「数えた範囲」と「申告した範囲」を一致させる。
    let scan = match collect_reference_scan(&canonical_dir, files) {
        Ok(scan) => scan,
        Err(_) => return DeadCodeDetection::without_dead(generated_candidates_skipped),
    };
    let unanalyzable = scan.unanalyzable_truncations(&canonical_dir);

    if candidates
        .all_syms
        .iter()
        .any(|(_, _, _, lang)| *lang == crate::language::LangId::Swift)
    {
        let liveness = SwiftProtocolLiveness::build(&canonical_dir, &scan.files, swift_protocols);
        exclude_swift_protocol_witnesses(&mut candidates, &liveness);
        if candidates.all_syms.is_empty() {
            return DeadCodeDetection::without_dead(generated_candidates_skipped);
        }
    }

    // テストからの参照かは `dir` からの相対パスで判定する。走査したファイルは絶対パスなので、
    // そのまま渡すとリポジトリの親ディレクトリ名 (`~/test/myrepo` の `test`) まで照合し、
    // 全参照をテストからの参照と数えていた (本番で使うシンボルが test_only になる)。
    let is_test = |path: &std::path::Path| is_workspace_test_path(&canonical_dir, path);

    let index =
        build_dead_code_name_index(&candidates, &canonical_dir, files, &scan.files, &is_test);

    // production / test 別に refs カウント。test/ 配下のみで参照されるシンボルは
    // dead_symbols ではなく test_only_symbols として分離する (F5)。
    let ref_counts = match crate::engine::refs::count_non_definition_refs_split_in_files(
        &index.unique_names,
        &scan.files,
        is_test,
    ) {
        Ok(v) => v,
        Err(_) => return DeadCodeDetection::without_dead(generated_candidates_skipped),
    };
    let unobserved = UnobservedReferences::new(&index.unique_names, &ref_counts.uncounted);

    let asset_refs = collect_framework_asset_refs(&canonical_dir);
    let default_liveness =
        DefaultExportLiveness::build(&candidates.all_syms, &canonical_dir, &scan.files);
    let rust_member_liveness =
        RustMemberLiveness::build(&candidates.all_syms, &canonical_dir, &scan.files, is_test);

    let mut classified = classify_dead_symbols(
        &candidates,
        &index,
        &ref_counts.counts,
        &asset_refs,
        &default_liveness,
        &rust_member_liveness,
        &unobserved,
    );
    attach_declaration_lines(dir, &mut classified.dead, &mut classified.test_only);
    attach_declaration_lines(dir, &mut classified.unverified, &mut []);
    let uncounted = uncounted_truncations(
        &canonical_dir,
        &ref_counts.uncounted,
        default_liveness.unobserved_files(),
        classified.unverified.len(),
    );
    DeadCodeDetection {
        dead: classified.dead,
        test_only: classified.test_only,
        generated_candidates_skipped,
        unanalyzable,
        uncounted,
        unverified: classified.unverified,
    }
}

fn exclude_swift_protocol_witnesses(
    candidates: &mut DeadCodeCandidates,
    liveness: &SwiftProtocolLiveness,
) {
    // Unsupported owners still contribute candidates. Never let a witness
    // key shared with one of those declarations remove both methods.
    let mut identities = std::collections::HashMap::new();
    for ((name, _, file, _), signature) in
        candidates.all_syms.iter().zip(&candidates.swift_signatures)
    {
        if let Some(signature) = signature {
            *identities
                .entry((file.clone(), name.clone(), signature.clone()))
                .or_insert(0) += 1;
        }
    }
    let mut kept = Vec::with_capacity(candidates.all_syms.len());
    let mut lines = Vec::with_capacity(candidates.all_syms.len());
    for (candidate, signature) in candidates
        .all_syms
        .drain(..)
        .zip(&candidates.swift_signatures)
    {
        let (name, _, file, _) = &candidate;
        if signature.as_deref().is_some_and(|signature| {
            identities.get(&(file.clone(), name.clone(), signature.to_owned())) == Some(&1)
                && liveness.contains(file, name, signature)
        }) {
            continue;
        }
        lines.push(
            signature
                .as_deref()
                .and_then(|signature| liveness.line(file, name, signature)),
        );
        kept.push(candidate);
    }
    candidates.all_syms = kept;
    candidates.swift_lines = lines;
}

#[cfg(test)]
mod swift_protocol_candidate_tests {
    use super::*;

    #[test]
    fn swift_protocol_duplicate_headers_include_unsupported_owners() {
        let temporary = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temporary.path()).unwrap();
        let path = root.join("Point.swift");
        std::fs::write(&path, "public struct A: Hashable {}\nextension A {\n public func hash(into value: inout Hasher) {}\n}\npublic struct B<T> {}\nextension B where T: Equatable {\n public func hash(into value: inout Hasher) {}\n}\n").unwrap();
        let files = vec![path];
        let dir = root.to_str().unwrap();
        let mut candidates = collect_dead_code_candidates(dir, &root, &files, false);
        assert_eq!(
            candidates
                .all_syms
                .iter()
                .filter(|(name, _, _, _)| name == "hash")
                .count(),
            2
        );
        let liveness =
            SwiftProtocolLiveness::build(&root, &files, &SwiftProtocolMethods::default());
        exclude_swift_protocol_witnesses(&mut candidates, &liveness);
        assert_eq!(
            candidates
                .all_syms
                .iter()
                .filter(|(name, _, _, _)| name == "hash")
                .count(),
            2,
            "unsupported owner's helper must not share the witness exclusion"
        );
    }
}

/// 参照を観測できなかった範囲。dead / test_only の断定を名前ごとに控えるために使う。
///
/// 件数集計で読み込み・構文解析に失敗したファイルからの参照は数えられていない。そのファイルに
/// 現れうる名前の候補を「参照が無い」と断定すると、唯一の呼び出しが読めないファイル
/// (100MB の上限超過など) にある関数を dead と報告する (旧実装は失敗を黙って捨てていた)。
/// production のファイルなら dead も test_only も、test のファイルなら dead だけを控える。
#[derive(Default)]
struct UnobservedReferences {
    /// production のファイルから参照されているかもしれない名前 (正規化済みのキー)。
    prod_names: HashSet<String>,
    /// test のファイルから参照されているかもしれない名前。
    test_names: HashSet<String>,
    /// 中身を確かめられない production / test のファイルがあり、どの名前も参照されうる。
    any_prod: bool,
    any_test: bool,
}

impl UnobservedReferences {
    fn new(names: &[String], uncounted: &[crate::engine::refs::UncountedFile]) -> Self {
        let mut unobserved = Self::default();
        for file in uncounted {
            match &file.possible_names {
                None if file.is_test => unobserved.any_test = true,
                None => unobserved.any_prod = true,
                Some(indices) => {
                    let bucket = if file.is_test {
                        &mut unobserved.test_names
                    } else {
                        &mut unobserved.prod_names
                    };
                    bucket.extend(indices.iter().filter_map(|&ix| names.get(ix).cloned()));
                }
            }
        }
        unobserved
    }

    /// `keys` のどれかが、数えられなかった production / test のファイルから参照されうるか。
    fn may_be_referenced(&self, keys: &[&str]) -> (bool, bool) {
        (
            self.any_prod || keys.iter().any(|key| self.prod_names.contains(*key)),
            self.any_test || keys.iter().any(|key| self.test_names.contains(*key)),
        )
    }
}

/// 参照を数えられなかったファイルの申告を作る (パス順、上限付き)。
///
/// 件数集計で失敗したファイルと、default export の判定で失敗したファイルを合わせる
/// (同じファイルは 1 件にまとめる)。`withheld` は断定を保留した候補の件数で、
/// 先頭の申告に添える。
fn uncounted_truncations(
    canonical_dir: &std::path::Path,
    uncounted: &[crate::engine::refs::UncountedFile],
    default_unobserved: &super::dead_code_default_liveness::UnobservedFiles,
    withheld: usize,
) -> Vec<crate::models::truncation::TruncationInfo> {
    use crate::models::truncation::TruncationInfo;

    let mut entries: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for file in uncounted {
        let rel = normalize_workspace_separators(
            &file
                .path
                .strip_prefix(canonical_dir)
                .unwrap_or(&file.path)
                .to_string_lossy(),
        );
        let scope = match &file.possible_names {
            Some(names) if names.is_empty() => {
                "no candidate name appears in it, so no judgement was withheld".to_string()
            }
            Some(names) => format!(
                "{} candidate name(s) appear in it, so they are not reported as dead",
                names.len()
            ),
            None => {
                "its content could not be scanned, so no candidate is reported as dead".to_string()
            }
        };
        entries.insert(
            rel,
            format!(
                "references in this file were not counted ({}); {scope}",
                file.cause
            ),
        );
    }
    for (rel, cause) in default_unobserved
        .prod
        .iter()
        .chain(default_unobserved.test.iter())
    {
        entries.entry(rel.clone()).or_insert_with(|| {
            format!(
                "default imports in this file were not counted ({cause}); \
                 default-export candidates are not reported as dead"
            )
        });
    }
    let total = entries.len();
    let mut out: Vec<TruncationInfo> = entries
        .into_iter()
        .take(UNCOUNTED_TRUNCATION_CAP)
        .map(|(rel, message)| TruncationInfo::source_analysis_failed(&rel, message))
        .collect();
    if total > UNCOUNTED_TRUNCATION_CAP {
        out.push(TruncationInfo::new(
            None,
            crate::models::truncation::TruncationReason::SourceAnalysisFailed,
            format!(
                "{} more file(s) whose references were not counted",
                total - UNCOUNTED_TRUNCATION_CAP
            ),
        ));
    }
    if let Some(first) = out.first_mut()
        && withheld > 0
    {
        first.message.push_str(&format!(
            " ({withheld} candidate(s) in total were withheld from dead/test-only)"
        ));
    }
    out
}

/// 参照を数えられなかったファイルの申告の上限 (残りは件数だけの 1 件にまとめる)。
const UNCOUNTED_TRUNCATION_CAP: usize = 10;

/// 分類の結果。
#[derive(Default)]
struct ClassifiedDead {
    dead: Vec<DeadSymbol>,
    test_only: Vec<DeadSymbol>,
    /// 件数だけなら dead / test_only だが、数えられなかった参照がありうるので保留した候補。
    unverified: Vec<DeadSymbol>,
}

impl ClassifiedDead {
    /// production の参照が 0 件の候補を、test の参照の有無と未観測の参照に応じて振り分ける。
    ///
    /// `unobserved` は (production, test) のファイルで数えられなかった参照がありうるか。
    /// test_only は production の参照が無いことの断定なので production 側だけを、
    /// dead は参照が 1 件も無いことの断定なので両方を見る。
    fn push(&mut self, sym: DeadSymbol, has_test_refs: bool, unobserved: (bool, bool)) {
        let (prod_unobserved, test_unobserved) = unobserved;
        if has_test_refs {
            if prod_unobserved {
                self.unverified.push(sym);
            } else {
                self.test_only.push(sym);
            }
        } else if prod_unobserved || test_unobserved {
            self.unverified.push(sym);
        } else {
            self.dead.push(sym);
        }
    }
}

/// dead-code の参照集計が走査するファイル集合 (count 経路と member liveness 経路で共有)。
///
/// `--glob` や既定のディレクトリ除外は候補の絞り込みであって参照の範囲ではないので適用しない。
/// 生成物も含め (`FileScanOptions::DEAD_CODE_REFERENCES`)、hidden ディレクトリ配下でも
/// 候補になった diff 由来ファイル (`extra_files`) は合流させる。
pub(crate) fn collect_reference_scan(
    canonical_dir: &std::path::Path,
    extra_files: &[std::path::PathBuf],
) -> Result<crate::engine::refs::FileCollection> {
    let mut scan = crate::engine::refs::collect_files_scan(
        canonical_dir,
        None,
        crate::engine::refs::FileScanOptions::DEAD_CODE_REFERENCES,
    )?;
    crate::engine::refs::merge_extra_files(&mut scan.files, canonical_dir, extra_files);
    Ok(scan)
}

/// 検出済み dead / test-only シンボルに宣言行を付与する。
///
/// `file` だけの報告は結局利用者が対象を探し直すことになるため行まで返す。
/// 行の表は per-file で 1 度だけ作り、対象ファイル数ぶんの parse に抑える
/// (dead が 0 件のファイルは parse しない)。
fn attach_declaration_lines(dir: &str, dead: &mut [DeadSymbol], test_only: &mut [DeadSymbol]) {
    use std::collections::HashMap;
    let mut cache: HashMap<String, DeclarationLines> = HashMap::new();
    for sym in dead.iter_mut().chain(test_only.iter_mut()) {
        let lines = cache
            .entry(sym.file.clone())
            .or_insert_with(|| DeclarationLines::load(dir, &sym.file));
        if sym.line.is_none() {
            sym.line = lines.line_of(&sym.name);
        }
    }
}

/// ファイル内のシンボルの宣言行 (0 始まり) の表。
///
/// dead / test-only の名前は class member だと qualname (`Container.member`) になる。
/// 末尾の名前だけをキーにした表では同じファイルの同名メソッド (`Alpha.fmt` と `Beta.fmt`)
/// が 1 つに潰れ、別のメソッドの行を報告していた。`--dead-scope touched-symbols` の判定も
/// その行で行うため、触った dead を落とし、触っていない dead で hook を止める逆向きになる。
#[derive(Default)]
struct DeclarationLines {
    /// container を持つシンボルは `Container.name`、持たないものは `name` → 宣言行 (先勝ち)。
    by_qualname: std::collections::HashMap<String, usize>,
    /// 末尾の名前 → (最初の宣言行, 同名の件数)。
    by_bare: std::collections::HashMap<String, (usize, usize)>,
}

impl DeclarationLines {
    fn load(dir: &str, file_path: &str) -> Self {
        let mut lines = Self::default();
        for symbol in extract_file_symbols(dir, file_path).unwrap_or_default() {
            // 宣言を共有する分割代入の束縛は、宣言の先頭ではなく名前の行を使う。
            let line = symbol.name_line();
            let qualname = match &symbol.container {
                Some(container) => format!("{container}.{}", symbol.name),
                None => symbol.name.clone(),
            };
            lines.by_qualname.entry(qualname).or_insert(line);
            lines.by_bare.entry(symbol.name).or_insert((line, 0)).1 += 1;
        }
        lines
    }

    /// `name` (qualname または bare name) の宣言行。qualname で引けないときは、同名が
    /// ファイルに 1 つだけの場合に限り末尾の名前で引く (複数あると別のシンボルの行になる)。
    fn line_of(&self, name: &str) -> Option<usize> {
        self.by_qualname
            .get(name)
            .copied()
            .or_else(|| match self.by_bare.get(bare_name(name)) {
                Some(&(line, 1)) => Some(line),
                _ => None,
            })
    }
}

/// `dir` 相対の `file_path` からシンボルを抽出する (container 付き)。
/// 宣言行の表にしか使わないので、循環的複雑度は計算しない。
fn extract_file_symbols(dir: &str, file_path: &str) -> Option<Vec<crate::models::symbol::Symbol>> {
    let full = std::path::Path::new(dir).join(file_path);
    let utf8 = camino::Utf8Path::new(full.to_str()?);
    let source = parser::read_file(utf8).ok()?;
    let lang_id = parser::detect_lang(utf8, &source).ok()?;
    if let crate::language::DetectedLang::LexerOnly(lexer_lang) = lang_id.detected() {
        return Some(crate::engine::lexer::extract_symbols(&source, lexer_lang));
    }
    let tree = parser::parse_source(&source, lang_id).ok()?;
    crate::engine::symbols::extract_symbols_without_complexity(tree.root_node(), &source, lang_id)
        .ok()
}

/// 候補収集の中間状態。dead-code 候補シンボルと C/C++ liveness 補助情報を保持する。
struct DeadCodeCandidates {
    /// 全ファイルのエクスポートシンボル (original_name, kind, file, lang_id)。
    /// case-insensitive 言語では lang_id でシンボル名を正規化した比較を行うため lang も保持する。
    all_syms: Vec<(String, String, String, crate::language::LangId)>,
    swift_signatures: Vec<Option<String>>,
    swift_lines: Vec<Option<usize>>,
    /// C/C++ の追加 liveness 情報 (file, シンボル名, 追加名リスト, lang)。
    /// enum→列挙子名 / typedef tag→alias 名。後段で正規化して liveness_aliases に変換する。
    liveness_raw: Vec<(String, String, Vec<String>, crate::language::LangId)>,
    /// 生成物として候補から外したファイル (`dir` 相対、`/` 区切り)。
    generated_skipped: Vec<String>,
}

/// 走査対象ファイルからエクスポートシンボル（trait impl メソッドは除外）と
/// C/C++ liveness 補助情報を収集する。
///
/// 生成物は既定で候補から外す (生成コードの未使用 export を dead として並べてもノイズになる)。
/// 外したファイルは申告用に記録する。`include_generated` のときは候補に含める。
///
/// ファイルごとの読み込み・parse・抽出は互いに独立しているので並列に行い、結果は
/// 入力の順に組み立てる (直列で処理したときと同じ順序になる)。
fn collect_dead_code_candidates(
    dir: &str,
    canonical_dir: &std::path::Path,
    files: &[std::path::PathBuf],
    include_generated: bool,
) -> DeadCodeCandidates {
    use rayon::prelude::*;

    // .gitattributes の linguist-generated 指定ファイルは dead-code 検出から除外する
    let gitattrs = crate::engine::gitattributes::GitAttributes::load(canonical_dir);
    let scan = |path: &std::path::PathBuf| {
        scan_candidate_file(dir, canonical_dir, path, &gitattrs, include_generated)
    };
    // 参照の集計と同じ上限付きのプールで回す (`ASTRO_SIGHT_BATCH_WORKERS` で低メモリ環境向けに
    // 並列度を下げられる)。プールを作れなければ直列に処理する (結果は同じ)。
    let scanned: Vec<CandidateFile> = match crate::engine::refs::build_bounded_pool() {
        Ok(pool) => pool.install(|| files.par_iter().map(scan).collect()),
        Err(_) => files.iter().map(scan).collect(),
    };

    let mut all_syms: Vec<(String, String, String, crate::language::LangId)> = Vec::new();
    let mut liveness_raw: Vec<(String, String, Vec<String>, crate::language::LangId)> = Vec::new();
    let mut generated_skipped: Vec<String> = Vec::new();
    let mut swift_signatures = Vec::new();
    for file in scanned {
        match file {
            CandidateFile::Ignored => {}
            CandidateFile::Generated(rel) => generated_skipped.push(rel),
            CandidateFile::Extracted(rel, candidates) => {
                let FileCandidates {
                    lang,
                    symbols,
                    cpp_liveness,
                } = candidates;
                for (name, kind, signature) in symbols {
                    swift_signatures
                        .push((lang == crate::language::LangId::Swift).then_some(signature));
                    all_syms.push((name, kind, rel.clone(), lang));
                }
                for (sym, extras) in cpp_liveness {
                    liveness_raw.push((rel.clone(), sym, extras, lang));
                }
            }
        }
    }

    DeadCodeCandidates {
        all_syms,
        swift_signatures,
        swift_lines: Vec::new(),
        liveness_raw,
        generated_skipped,
    }
}

/// 候補抽出の 1 ファイルぶんの結果。
enum CandidateFile {
    /// 削除済み・`dir` の外・抽出できないなどで候補を持たないファイル。
    Ignored,
    /// 生成物として候補から外したファイル (`dir` 相対、`/` 区切り)。
    Generated(String),
    /// 候補を抽出したファイル (`dir` 相対、`/` 区切り) とその候補。
    Extracted(String, FileCandidates),
}

/// `collect_dead_code_candidates` の 1 ファイルぶんの処理。
fn scan_candidate_file(
    dir: &str,
    canonical_dir: &std::path::Path,
    path: &std::path::Path,
    gitattrs: &crate::engine::gitattributes::GitAttributes,
    include_generated: bool,
) -> CandidateFile {
    // canonicalize で削除済みファイルをスキップ、dir 外のパスも除外
    let Ok(canonical_path) = std::fs::canonicalize(path) else {
        return CandidateFile::Ignored;
    };
    // diff 由来のパス (常に `/` 区切り) と突き合わせるため区切り文字を正規化する。
    // そろえないと filter_dead_by_touched_symbols / filter_dead_by_wip_added の突合せが
    // Windows で常に不一致になり、前者は dead を全件落とし (= review --hook の既定スコープで
    // dead 検出が丸ごと消える)、後者は WIP 抑止が効かなくなる。
    let rel = match canonical_path.strip_prefix(canonical_dir) {
        Ok(p) => normalize_workspace_separators(&p.to_string_lossy()),
        Err(_) => return CandidateFile::Ignored, // dir 外のパスは除外（セキュリティ境界）
    };
    // ファイル先頭の「自動生成」マーカーコメントでも除外する (.gitattributes が
    // 無いリポジトリでも tree-sitter の parser.c / protoc の *.pb.go 等を無視できる)
    if !include_generated
        && (gitattrs.is_generated(&rel)
            || crate::engine::generated::is_auto_generated(&canonical_path))
    {
        return CandidateFile::Generated(rel);
    }
    match extract_file_candidates(dir, &rel) {
        Some(candidates) => CandidateFile::Extracted(rel, candidates),
        None => CandidateFile::Ignored,
    }
}

/// 1 ファイルぶんの dead-code 候補。
struct FileCandidates {
    lang: crate::language::LangId,
    /// エクスポートシンボル (name, kind, signature)。trait impl メソッドと
    /// フレームワークの実行時入口は除外済み。
    symbols: ExportedSymbols,
    /// C/C++ の liveness 補助情報 (シンボル名, 追加名リスト)。他の言語では空。
    cpp_liveness: Vec<(String, Vec<String>)>,
}

/// `dir` 相対の `file_path` から dead-code の候補を抽出する。
///
/// 読み込み・言語判定・parse は 1 回だけ行う。C/C++ の liveness 補助情報 (enum の列挙子・
/// typedef alias) も同じ構文木から集める (旧実装は候補抽出とは別に読み直して parse していた)。
/// enum 型名が直接使われなくても列挙子が使われていれば live、body あり typedef tag が alias 名で
/// のみ使われていても live と判定するために使う (Issue #11/#12)。
///
/// 候補は名前・種別・シグネチャしか使わないので、循環的複雑度は計算しない。
/// それ以外は `extract_exported_symbols_from_file_inner_with_lang(dir, file_path, true, true)`
/// と同じ候補を返す。
fn extract_file_candidates(dir: &str, file_path: &str) -> Option<FileCandidates> {
    // `../etc/passwd` 等のトラバーサルや絶対パスを拒否し、workspace 外のファイルを読まない。
    if !crate::engine::impact::is_safe_diff_path(file_path) {
        return None;
    }
    let full_path = std::path::Path::new(dir).join(file_path);
    let utf8_path = camino::Utf8Path::new(full_path.to_str()?);
    let source = parser::read_file(utf8_path).ok()?;
    let lang = parser::detect_lang(utf8_path, &source).ok()?;

    // lexer-only 言語 (現状 Xojo) は tree-sitter を持たないため、lexer 経由で
    // export 相当のシンボルを抽出する。
    if let crate::language::DetectedLang::LexerOnly(lexer_lang) = lang.detected() {
        return Some(FileCandidates {
            lang,
            symbols: crate::engine::lexer::extract_exported_symbols(&source, lexer_lang, true),
            cpp_liveness: Vec::new(),
        });
    }

    let tree = parser::parse_source(&source, lang).ok()?;
    let root = tree.root_node();
    let syms =
        crate::engine::symbols::extract_symbols_without_complexity(root, &source, lang).ok()?;
    let symbols = filter_exported_symbols(&syms, root, &source, lang, true, true, Some(file_path));
    let cpp_liveness = if matches!(
        lang,
        crate::language::LangId::C | crate::language::LangId::Cpp
    ) {
        crate::engine::symbols::collect_cpp_dead_liveness_aliases(root, &source, lang)
    } else {
        Vec::new()
    };
    Some(FileCandidates {
        lang,
        symbols,
        cpp_liveness,
    })
}

/// 検索インデックスの中間状態。正規化済み bare name の同名カウント / liveness alias / 検索対象名。
struct DeadCodeNameIndex {
    /// 正規化 bare name → 出現数。同名 export が複数あるシンボルの保守的スキップに使う。
    name_counts: std::collections::HashMap<String, usize>,
    /// C/C++ の (file, 正規化シンボル名) → 追加 liveness 名 (正規化済み)。
    liveness_aliases: std::collections::HashMap<(String, String), Vec<String>>,
    /// refs 検索対象の正規化済みシンボル名 (重複除去済み、liveness 補助名を含む)。
    unique_names: Vec<String>,
    /// TS/JS の duplicate な同名 class member について owner 一意推定で
    /// dead/test_only/live を再判定するためのインデックス。`name_counts > 1` で
    /// スキップ対象の候補のうち、本インデックスで `Live` / `TestOnly` / `Dead` が
    /// 返るものは旧スキップを緩和して通常の dead 判定経路に乗せる。
    member_liveness: JsTsMemberLiveness,
    /// PHP の duplicate な同名 class member について `Owner::member()` などの
    /// 確定参照だけを使って dead/test_only/live を再判定するためのインデックス。
    php_member_liveness: PhpMemberLiveness,
    /// Ruby の静的な new から通常の initialize への追加参照。Dead の断定には使わない。
    ruby_constructor_liveness: RubyConstructorLiveness,
}

/// 言語別に正規化した bare name を返す。
///
/// refs 検索は AST 上の identifier ノードに対してマッチするため、
/// `Container.method` 形式の qualname ではマッチせず常に 0 件となってしまう。
/// そのため検索キーは末尾セグメント（bare name）に統一する。
///
/// Bash は qualname を作らない一方、関数名に `.` を使える (`utils.log() { … }`)。
/// 末尾で畳むと呼び出し `utils.log` を `log` で探して 0 件になり、呼ばれている関数を
/// dead と報告していたので、名前全体を検索キーにする (API 差分の参照索引と同じ扱い)。
fn normalized_bare_name(lang: crate::language::LangId, name: &str) -> String {
    let key = if lang == crate::language::LangId::Bash {
        name
    } else {
        bare_name(name)
    };
    crate::language::normalize_identifier(lang, key).into_owned()
}

/// 候補シンボルから同名カウント / liveness alias / refs 検索対象名のインデックスを構築する。
/// `candidate_files` は diff 由来の候補ファイル (hidden 配下含む)。member liveness の
/// 走査集合は count 経路と同じ `collect_reference_scan` で作る。
fn build_dead_code_name_index<F>(
    candidates: &DeadCodeCandidates,
    canonical_dir: &std::path::Path,
    candidate_files: &[std::path::PathBuf],
    reference_files: &[std::path::PathBuf],
    is_test: &F,
) -> DeadCodeNameIndex
where
    F: Fn(&std::path::Path) -> bool + Sync,
{
    // 同名 export が複数ファイル/複数コンテナに存在する場合は保守的にスキップ（誤判定防止）。
    // キーは bare name を言語別に正規化したもの (Xojo では `Foo` と `FOO` を同一視)。
    let mut name_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for (name, _, _, lang) in &candidates.all_syms {
        *name_counts
            .entry(normalized_bare_name(*lang, name))
            .or_default() += 1;
    }

    // C/C++ の (file, 正規化シンボル名) → 追加 liveness 名 (正規化済み) を構築。
    // enum 候補は列挙子名、typedef tag 候補は alias 名を介した参照でも live と判定する。
    let mut liveness_aliases: std::collections::HashMap<(String, String), Vec<String>> =
        std::collections::HashMap::new();
    for (file, sym, extras, lang) in &candidates.liveness_raw {
        let key = normalized_bare_name(*lang, sym);
        let extra_keys: Vec<String> = extras
            .iter()
            .map(|e| normalized_bare_name(*lang, e))
            .collect();
        liveness_aliases
            .entry((file.clone(), key))
            .or_default()
            .extend(extra_keys);
    }

    // 全シンボル名の非 Definition 参照件数をカウント（SymbolReference を確保しない）。
    // 入力も正規化済みキーで渡し、refs 側の HashMap キーと lookup を一致させる。
    // liveness 補助名 (列挙子 / alias) も検索対象に含め、enum/tag の生存判定に使う。
    let unique_names: Vec<String> = {
        let mut seen = HashSet::new();
        let mut names = Vec::new();
        for (name, _, _, lang) in &candidates.all_syms {
            let k = normalized_bare_name(*lang, name);
            if seen.insert(k.clone()) {
                names.push(k);
            }
        }
        for extras in liveness_aliases.values() {
            for ek in extras {
                if seen.insert(ek.clone()) {
                    names.push(ek.clone());
                }
            }
        }
        names
    };

    // TS/JS の duplicate な同名 class member について owner 一意推定で liveness を再判定する。
    // `name_counts > 1` で従来スキップされていた候補のうち、安全に owner を一意推定できる
    // ケースだけ通常の dead/test_only/live 判定経路に乗せる。
    let member_liveness = JsTsMemberLiveness::build(
        &candidates.all_syms,
        canonical_dir,
        candidate_files,
        is_test,
    );
    let php_member_liveness = PhpMemberLiveness::build(
        &candidates.all_syms,
        canonical_dir,
        candidate_files,
        is_test,
    );
    let ruby_constructor_liveness = RubyConstructorLiveness::build(
        &candidates.all_syms,
        canonical_dir,
        reference_files,
        is_test,
    );

    DeadCodeNameIndex {
        name_counts,
        liveness_aliases,
        unique_names,
        member_liveness,
        php_member_liveness,
        ruby_constructor_liveness,
    }
}

/// AST の cross-file refs では追跡できないフレームワークアセット由来の参照名集合。
struct FrameworkAssetRefs {
    /// AndroidManifest.xml / layout XML から参照されるシンボル名。
    xml: HashSet<String>,
    /// Angular テンプレート (`*.component.html` / inline template) から参照される名前。
    template: HashSet<String>,
}

impl FrameworkAssetRefs {
    /// bare name / qualname のいずれかでフレームワークアセットから参照されているか判定する。
    fn contains_symbol(&self, name: &str) -> bool {
        // bare name と qualname (Container.method) の両方を突き合わせる。
        // layout XML の `android:onClick="handler"` や Angular template の
        // `(event)="handler()"` は単純名でしか書けないため bare で検索し、
        // `android:name=".Foo"` 等で Container 側をカバーするケースは qualname でも検査する。
        let bare = bare_name(name);
        self.xml.contains(bare)
            || self.xml.contains(name)
            || self.template.contains(bare)
            || self.template.contains(name)
    }
}

/// Android XML / Angular テンプレートからの参照集合を収集する。
fn collect_framework_asset_refs(canonical_dir: &std::path::Path) -> FrameworkAssetRefs {
    // Android プロジェクトでは `AndroidManifest.xml` / layout XML から
    // シンボルが参照されうる（`<activity android:name=".MainActivity"/>` 等）。
    // Kotlin/Java AST のみでは追跡できない Android framework 経由の生存判定を補うため、
    // XML 参照集合に含まれるシンボルは dead から除外する。
    // AndroidManifest.xml が存在しないプロジェクトでは空集合が返り副作用なし。
    let xml = crate::engine::xml_refs::collect_xml_symbol_references(canonical_dir);

    // Angular プロジェクトでは `*.component.html` テンプレートや
    // `@Component({ template: \`...\` })` の inline template 内の binding 式から
    // component method/プロパティが参照される。TypeScript AST のみでは追跡できない
    // ため、テンプレート参照集合に含まれるシンボルは dead から除外する。
    // angular.json / *.component.ts のどちらも見つからないプロジェクトでは空集合が
    // 返り副作用なし。
    let template =
        crate::engine::angular_template_refs::collect_angular_template_refs(canonical_dir);

    FrameworkAssetRefs { xml, template }
}

/// シンボルの production / test 参照数を返す。
///
/// C/C++ では enum 列挙子 / typedef alias (liveness alias) 経由の参照も合算する。
/// enum 型名が直接使われなくても列挙子が使われていれば live、body あり typedef tag が
/// alias 名でのみ使われていても live と判定する (Issue #11/#12)。
fn ref_counts_with_liveness(
    key: &str,
    file: &str,
    index: &DeadCodeNameIndex,
    counts: &std::collections::HashMap<String, (usize, usize)>,
) -> (usize, usize) {
    let (mut prod_cnt, mut test_cnt) = counts.get(key).copied().unwrap_or((0, 0));
    if let Some(extra_keys) = index
        .liveness_aliases
        .get(&(file.to_string(), key.to_string()))
    {
        for ek in extra_keys {
            if let Some((p, t)) = counts.get(ek) {
                prod_cnt += p;
                test_cnt += t;
            }
        }
    }
    (prod_cnt, test_cnt)
}

/// qualname `Container.member` を owner/bare に分割し、言語別 member liveness を
/// 呼び出す。対象外言語、または owner を一意特定できない名前 (bare のみ / 多段
/// qualname) の場合は `None` を返し、呼び出し側は従来の保守的スキップを維持する。
fn member_status_for_candidate(
    name: &str,
    file: &str,
    lang: crate::language::LangId,
    js_ts_liveness: &JsTsMemberLiveness,
    php_liveness: &PhpMemberLiveness,
    rust_liveness: &RustMemberLiveness,
) -> Option<MemberStatus> {
    let (owner, bare) = name.rsplit_once('.')?;
    if owner.contains('.') {
        // 多段 qualname は class.member とみなさない。
        return None;
    }
    match lang {
        crate::language::LangId::Typescript
        | crate::language::LangId::Javascript
        | crate::language::LangId::Tsx => js_ts_liveness.status_for(owner, bare, file),
        crate::language::LangId::Php => php_liveness.status_for(owner, bare, file),
        crate::language::LangId::Rust => rust_liveness.status_for(owner, bare, file),
        _ => None,
    }
}

/// 参照カウントと各除外規約から dead / test-only シンボルを分類する。
fn classify_dead_symbols(
    candidates: &DeadCodeCandidates,
    index: &DeadCodeNameIndex,
    counts: &std::collections::HashMap<String, (usize, usize)>,
    asset_refs: &FrameworkAssetRefs,
    default_liveness: &DefaultExportLiveness,
    rust_member_liveness: &RustMemberLiveness,
    unobserved: &UnobservedReferences,
) -> ClassifiedDead {
    // production 0 / test 0 → dead_symbols
    // production 0 / test > 0 → test_only_symbols (F5)
    // production > 0 → 生存とみなしどちらにも報告しない
    // 数えられなかったファイルから参照されうる候補は断定せず unverified に回す
    let mut classified = ClassifiedDead::default();
    for (candidate_index, (name, kind, file, lang)) in candidates.all_syms.iter().enumerate() {
        let key = normalized_bare_name(*lang, name);
        // 数えられなかったファイルからの参照がありうるか (検索キーと liveness 補助名、
        // default export の import)。(production, test) の組。
        let unobserved_refs = || {
            let mut keys: Vec<&str> = vec![key.as_str()];
            if let Some(extra) = index
                .liveness_aliases
                .get(&(file.to_string(), key.to_string()))
            {
                keys.extend(extra.iter().map(String::as_str));
            }
            let (prod, test) = unobserved.may_be_referenced(&keys);
            let (default_prod, default_test) =
                default_liveness.may_have_unobserved_uses(file, name);
            (prod || default_prod, test || default_test)
        };
        let symbol = || DeadSymbol {
            name: name.clone(),
            kind: kind.clone(),
            file: file.clone(),
            // 宣言行は attach_declaration_lines で後付けする
            // (per-file の parse を 1 回に集約するため)。
            line: candidates
                .swift_lines
                .get(candidate_index)
                .copied()
                .flatten(),
        };
        // 同名シンボルが複数存在する場合は bare name では区別できないためスキップする。
        // ただし TS/JS の class member については `JsTsMemberLiveness` で owner 一意推定
        // が成立した場合のみ、duplicate スキップを緩和して通常判定に乗せる。
        let member_status = member_status_for_candidate(
            name,
            file,
            *lang,
            &index.member_liveness,
            &index.php_member_liveness,
            rust_member_liveness,
        );
        if index.name_counts.get(&key).copied().unwrap_or(0) > 1
            || (*lang == crate::language::LangId::Rust
                && matches!(
                    member_status,
                    Some(MemberStatus::Live | MemberStatus::TestOnly | MemberStatus::Dead)
                ))
        {
            match member_status {
                Some(MemberStatus::Live) => continue,
                Some(status @ (MemberStatus::TestOnly | MemberStatus::Dead)) => {
                    // Angular template / Android XML から参照されるシンボルは live。
                    if asset_refs.contains_symbol(name) {
                        continue;
                    }
                    classified.push(
                        symbol(),
                        status == MemberStatus::TestOnly,
                        unobserved_refs(),
                    );
                    continue;
                }
                Some(MemberStatus::Ambiguous) | None => continue,
            }
        }

        let (prod_cnt, test_cnt) = ref_counts_with_liveness(&key, file, index, counts);
        let (default_prod, default_test) = default_liveness.counts_for(file, name);
        let (constructor_prod, constructor_test) = if *lang == crate::language::LangId::Ruby {
            index.ruby_constructor_liveness.counts_for(file, name)
        } else {
            (0, 0)
        };
        let prod_cnt = prod_cnt
            .saturating_add(default_prod)
            .saturating_add(constructor_prod);
        let test_cnt = test_cnt
            .saturating_add(default_test)
            .saturating_add(constructor_test);
        if prod_cnt > 0 {
            continue;
        }

        // Android XML / Angular テンプレート経由で参照されるシンボルは live。
        if asset_refs.contains_symbol(name) {
            continue;
        }

        // PHPUnit テストクラス内のヘルパーメソッドは test_only からも除外する。
        if test_cnt > 0 && has_phpunit_test_container(name, *lang) {
            continue;
        }
        classified.push(symbol(), test_cnt > 0, unobserved_refs());
    }

    classified
}

/// テストディレクトリとみなすセグメント名一覧。
///
/// - 言語共通: `tests`, `Tests`, `__tests__`, `spec`, `testdata`
/// - JVM/Gradle 標準: `test` (`src/test/`), `androidTest`, `sharedTest`, `integrationTest`
///
/// `is_test_path` (API 差分検出) と `DEFAULT_DEAD_CODE_EXCLUDES_TESTS` (dead-code 既定除外)
/// の両側で同じ判定を行うため一元化する。`is_test_path` が `test` 単数形を含む一方で
/// `DEFAULT_DEAD_CODE_EXCLUDES_TESTS` には含まれない、という履歴的なねじれ
/// (2026-05-21 の JUnit Kotlin dead 誤検出として顕在化) を解消する。
pub(crate) const TEST_DIRECTORY_SEGMENTS: &[&str] = &[
    "tests",
    "test",
    "Tests",
    "__tests__",
    "spec",
    "testdata",
    "androidTest",
    "sharedTest",
    "integrationTest",
];

/// `canonical_dir` 配下の絶対パスについて、`dir` からの相対パスでテストのファイルか判定する。
///
/// 親ディレクトリの名前 (`/builds/test/<project>` の `test`) はリポジトリ内のファイルが
/// テストかどうかと関係ない。ディレクトリ除外 (`refs/files.rs`) と
/// `filter_diff_files_for_dead_code` も同じ理由で `dir` 相対にしている。
/// `canonical_dir` の外のパスは従来どおりそのまま判定する。
pub(crate) fn is_workspace_test_path(
    canonical_dir: &std::path::Path,
    path: &std::path::Path,
) -> bool {
    is_test_path(path.strip_prefix(canonical_dir).unwrap_or(path))
}

/// refs カウントを production / test に振り分けるための判定関数。
///
/// - ファイル名規約 (`*_test.go`, `*Test.php`, `*_spec.rb` 等) は既存の
///   `is_test_file_path` に委譲する。
/// - ディレクトリセグメント規約は `TEST_DIRECTORY_SEGMENTS` に一元化。
///
/// 引数はファイルシステム上のパス (`dir` 相対、または絶対パス) なので、区切り文字はプラットフォーム依存
/// (Windows では `\`)。文字列を `/` 前提で分解すると Windows でどちらの規約にも
/// マッチせず、テストからの参照が production 参照として数えられてしまう
/// (`test_only_symbols` が常に空になり、テスト専用シンボルが live 扱いになる)。
/// そのため `Path::file_name` / `Path::components` で分解してから判定する。
pub(crate) fn is_test_path(path: &std::path::Path) -> bool {
    if let Some(name) = path.file_name().and_then(|s| s.to_str())
        && crate::engine::impact::test_context::is_test_file_path(name)
    {
        return true;
    }
    path.components().any(|c| {
        matches!(c, std::path::Component::Normal(_))
            && c.as_os_str()
                .to_str()
                .is_some_and(|seg| TEST_DIRECTORY_SEGMENTS.contains(&seg))
    })
}

#[cfg(test)]
pub(crate) fn extract_dead_code_candidates_from_file(
    dir: &str,
    file_path: &str,
) -> Option<Vec<(String, String, String)>> {
    // dead-code 走査では既定でテストディレクトリ (tests/, Tests/, __tests__/, spec/,
    // testdata/) が collect 段階で除外される。`--include-tests` で opt-in したときは
    // テストファイルも走査対象に含めるため、ここでは test_path 除外を行わない
    // (API 検出側 extract_exported_symbols_from_file は test path 除外を行う)。
    //
    // dead-code 判定では Typer / Click / FastAPI / Flask / pytest 等のフレームワーク
    // 登録デコレータが付いた関数を除外する。デコレータ経由でフレームワーク内部
    // レジストリに登録されるため、識別子レベルの cross-file refs では caller を
    // 追跡できず偽陽性源になる。
    extract_file_candidates(dir, file_path).map(|candidates| candidates.symbols)
}

pub(crate) fn filter_dead_by_touched_symbols(
    dir: &str,
    dead: Vec<crate::models::review::DeadSymbol>,
    diff_input: &str,
    diff_files: &[crate::models::impact::DiffFile],
) -> Vec<crate::models::review::DeadSymbol> {
    use std::collections::{HashMap, HashSet};

    // changed file 集合 (削除ファイルは含めない)。
    let mut changed_files: HashSet<&str> = HashSet::new();
    for df in diff_files {
        if df.new_path != "/dev/null" {
            changed_files.insert(df.new_path.as_str());
        }
    }

    // 「ファイル → 追加行 set (0-indexed)」「ファイル → シンボル名→宣言行」を per-file キャッシュ。
    let mut changed_lines_cache: HashMap<String, HashSet<usize>> = HashMap::new();
    let mut sym_lines_cache: HashMap<String, DeclarationLines> = HashMap::new();

    dead.into_iter()
        .filter(|ds| {
            if !changed_files.contains(ds.file.as_str()) {
                // diff に含まれないファイル: touched ではないので除外。
                return false;
            }
            let changed_lines = changed_lines_cache
                .entry(ds.file.clone())
                .or_insert_with(|| {
                    crate::engine::diff::extract_changed_new_lines(diff_input, &ds.file)
                });
            let lines = sym_lines_cache
                .entry(ds.file.clone())
                .or_insert_with(|| DeclarationLines::load(dir, &ds.file));
            // DeadSymbol.name は class member だと qualname (`Container.method`)。
            // qualname で引き、同名が 1 つだけのときに限り末尾の名前でも引く
            // (`DeclarationLines::line_of`)。同名の別メソッドの行で判定すると、
            // 触った dead を落とし、触っていない dead を残す逆向きになる。
            let Some(line) = lines.line_of(&ds.name) else {
                // 宣言行が引けない (lexer-only で取り漏れ等) は保守的に touched 扱いで残す。
                return true;
            };
            changed_lines.contains(&line)
        })
        .collect()
}

/// 同一 diff で新規 export された (= `api_changes.added` に列挙される) シンボルを
/// dead 警告から除外する。`review --hook` の既定挙動として「多段実装中に純粋ヘルパーを
/// 先に追加し、consumer 結線が後続コミットになる」WIP パターンのノイズを抑止するために
/// 使う。`--include-wip-dead` 指定時は呼び出し側でこの関数を skip し、新規 export も
/// dead として全件返す。
///
/// 突合せキーは `(file, name)` (DeadSymbol と ApiSymbol の共通フィールド)。kind は
/// 同一シンボルで言語別表記が一致しないケースを避けるため照合対象から外す
/// (例: `class` vs `struct` の混在は file+name の一致で十分判別できる)。
pub(crate) fn filter_dead_by_wip_added(
    dead: Vec<crate::models::review::DeadSymbol>,
    added: &[crate::models::review::ApiSymbol],
) -> Vec<crate::models::review::DeadSymbol> {
    if added.is_empty() {
        return dead;
    }
    let added_set: std::collections::HashSet<(&str, &str)> = added
        .iter()
        .map(|a| (a.file.as_str(), a.name.as_str()))
        .collect();
    dead.into_iter()
        .filter(|ds| !added_set.contains(&(ds.file.as_str(), ds.name.as_str())))
        .collect()
}

/// `sym` の range を内包する最も内側の container (class/struct/trait/interface/enum) を返す。
/// `sym` 自身は除外する。
pub(crate) fn enclosing_container<'a>(
    sym: &crate::models::symbol::Symbol,
    containers: &'a [&'a crate::models::symbol::Symbol],
) -> Option<&'a crate::models::symbol::Symbol> {
    containers
        .iter()
        .copied()
        .filter(|c| symbol_range_contains(c, sym) && !symbol_range_equal(c, sym))
        .min_by_key(|c| {
            (
                c.range.end.line.saturating_sub(c.range.start.line),
                c.range.end.column.saturating_sub(c.range.start.column),
            )
        })
}

fn symbol_range_contains(
    outer: &crate::models::symbol::Symbol,
    inner: &crate::models::symbol::Symbol,
) -> bool {
    let starts_before_or_at = outer.range.start.line < inner.range.start.line
        || (outer.range.start.line == inner.range.start.line
            && outer.range.start.column <= inner.range.start.column);
    let ends_after_or_at = outer.range.end.line > inner.range.end.line
        || (outer.range.end.line == inner.range.end.line
            && outer.range.end.column >= inner.range.end.column);
    starts_before_or_at && ends_after_or_at
}

fn symbol_range_equal(
    a: &crate::models::symbol::Symbol,
    b: &crate::models::symbol::Symbol,
) -> bool {
    a.range.start.line == b.range.start.line
        && a.range.start.column == b.range.start.column
        && a.range.end.line == b.range.end.line
        && a.range.end.column == b.range.end.column
}

// ---------------------------------------------------------------------------
// Dead-code コマンド: diff 関連 or プロジェクト全体のデッドコード検出
// ---------------------------------------------------------------------------

/// `dead-code` の既定除外ディレクトリ名。
///
/// 大規模リポでは `vendor/`, `node_modules/`, `tests/` 等が `dead-code` 候補の 88%+ を占め、
/// 実運用のノイズになる。ディレクトリ名と完全一致するセグメントをパスに含むファイルを
/// 走査対象から落とす。`--include-vendor` / `--include-tests` / `--include-build` で
/// 個別に再取込できる。
///
/// グループ化の意図:
/// - `vendor`: Composer, Ruby Bundler, Go modules vendor
/// - `node_modules`, `bower_components`: Node パッケージ
/// - `tests`, `Tests`, `__tests__`, `spec`, `testdata`,
///   `test`, `androidTest`, `sharedTest`, `integrationTest`: 言語共通 + JVM/Gradle のテストディレクトリ
///   (実体は `TEST_DIRECTORY_SEGMENTS` 定数で `is_test_path` と共有)
/// - `target`, `dist`, `build`, `out`, `_build`, `cmake-build-debug`, `cmake-build-release`: ビルド成果物
/// - `.venv`, `venv`, `.tox`: Python 仮想環境
pub(crate) const DEFAULT_DEAD_CODE_EXCLUDES_VENDOR: &[&str] = &[
    "vendor",
    "node_modules",
    "bower_components",
    ".venv",
    "venv",
    ".tox",
];
/// dead-code 既定除外のテストディレクトリ。`is_test_path` と同じセグメント集合
/// (`TEST_DIRECTORY_SEGMENTS`) を使い、API 検出側と dead-code 側のテスト判定を統一する。
pub(crate) const DEFAULT_DEAD_CODE_EXCLUDES_TESTS: &[&str] = TEST_DIRECTORY_SEGMENTS;
pub(crate) const DEFAULT_DEAD_CODE_EXCLUDES_BUILD: &[&str] = &[
    "target",
    "dist",
    "build",
    "out",
    "_build",
    "cmake-build-debug",
    "cmake-build-release",
];

/// 現在のフラグ設定から除外ディレクトリリストを組み立てる。
pub(crate) fn resolve_dead_code_excludes(
    include_vendor: bool,
    include_tests: bool,
    include_build: bool,
) -> Vec<&'static str> {
    let mut excludes: Vec<&'static str> = Vec::new();
    if !include_vendor {
        excludes.extend(DEFAULT_DEAD_CODE_EXCLUDES_VENDOR);
    }
    if !include_tests {
        excludes.extend(DEFAULT_DEAD_CODE_EXCLUDES_TESTS);
    }
    if !include_build {
        excludes.extend(DEFAULT_DEAD_CODE_EXCLUDES_BUILD);
    }
    excludes
}

/// Laravel 規約プリセット。フレームワークが自動で呼び出す規約的エントリポイントを除外する。
///
/// - `database/migrations/**`: Artisan `migrate` から `up()` / `down()` を呼ぶ
/// - `database/seeds/**` / `database/seeders/**` / `database/factories/**`: Artisan `db:seed` が `run()` を呼ぶ
/// - `database/views/**`: DB view 定義 (Artisan 駆動)
/// - `app/Console/Commands/**`: `handle()` が Artisan から呼ばれる
/// - `app/Http/Controllers/**`: Route 定義 (`routes/web.php` 等) から文字列経由で呼ばれる
/// - `app/Http/Middleware/**`: `handle()` が Route/Kernel 経由で呼ばれる
/// - `app/Http/Requests/**`: `authorize()` / `rules()` が Form Request 解決時に自動呼出し
/// - `app/Http/Resources/**`: `toArray()` が Response serialization で呼ばれる
/// - `app/GraphQL/**`: GraphQL schema ファイルから文字列経由で解決される
/// - `app/Listeners/**`, `app/Providers/**`: Service Container / Event Bus 経由
/// - `_ide_helper*.php`, `.phpstorm.meta.php`: IDE 補助の自動生成ファイル
///
/// `**/` 接頭辞でサブディレクトリに埋め込まれた Laravel アプリ（モノレポ内の複数 Laravel
/// 等）にも対応する。
pub(crate) const LARAVEL_PRESET_EXCLUDE_GLOBS: &[&str] = &[
    // 標準マイグレーション経路 (Artisan 駆動)
    "**/database/migrations/**",
    // Multi-DB / 複数コネクション構成で派生する migrations_foo, migrations-foo
    // (Laravel 公式 ドキュメントの `--path` 指定パターン) も同様に Artisan 駆動
    "**/database/migrations_*/**",
    "**/database/migrations-*/**",
    // シーダー / ファクトリ / ビュー定義 / テーブル定義スナップショット
    "**/database/seeds/**",
    "**/database/seeders/**",
    "**/database/factories/**",
    "**/database/views/**",
    "**/database/TableDefinitions/**",
    // Artisan / Route / GraphQL 経由で呼ばれるエントリポイント
    "**/app/Console/Commands/**",
    "**/app/Http/Controllers/**",
    "**/app/Http/Middleware/**",
    "**/app/Http/Requests/**",
    "**/app/Http/Resources/**",
    "**/app/GraphQL/**",
    "**/app/Listeners/**",
    "**/app/Providers/**",
    // bootstrap/app.php で ExceptionHandler 規約で登録されるハンドラ
    "**/app/Exceptions/**",
    // Service Container / Observer / Cast / Policy / Event / Queue / Mail / Notification /
    // Broadcast channel / FormRequest validation Rule — いずれも Laravel のフレームワーク側が
    // reflection / 文字列 FQN / 自動ディスパッチで呼び出す規約的エントリポイント群
    "**/app/Casts/**",
    "**/app/Observers/**",
    "**/app/Policies/**",
    "**/app/Events/**",
    "**/app/Jobs/**",
    "**/app/Notifications/**",
    "**/app/Mail/**",
    "**/app/Rules/**",
    "**/app/Broadcasting/**",
    // IDE 補助の自動生成ファイル
    "**/_ide_helper.php",
    "**/_ide_helper_models.php",
    "**/.phpstorm.meta.php",
];

/// Next.js (App Router / Pages Router) のフレームワーク entrypoint プリセット。
///
/// Next.js のファイルシステムルーティングでは、特定のファイル名 (`page` / `layout` /
/// `route` 等) の default export が Next.js ランタイム経由で呼ばれる。AST 上の
/// cross-file refs では caller を追跡できないため、astro-sight 単独では
/// `dead-code` の偽陽性源になる。`--framework nextjs` でこれらを除外する。
///
/// - **App Router** (Next.js 13+): `app/**/page.*`, `layout.*`, `loading.*`, `error.*`,
///   `not-found.*`, `template.*`, `default.*`, `global-error.*`, `route.*`
/// - **Pages Router** (legacy): `pages/**/*.{js,jsx,ts,tsx}` (含む `pages/api/**`)
/// - **Root entrypoints**: `middleware.{js,ts}`, `instrumentation.{js,ts}`
///
/// `src/app/**` のような src layout もそのまま `**/app/**` のグロブでカバーされる。
pub(crate) const NEXTJS_PRESET_EXCLUDE_GLOBS: &[&str] = &[
    // App Router 規約ファイル
    "**/app/**/page.{js,jsx,ts,tsx}",
    "**/app/**/layout.{js,jsx,ts,tsx}",
    "**/app/**/loading.{js,jsx,ts,tsx}",
    "**/app/**/error.{js,jsx,ts,tsx}",
    "**/app/**/not-found.{js,jsx,ts,tsx}",
    "**/app/**/template.{js,jsx,ts,tsx}",
    "**/app/**/default.{js,jsx,ts,tsx}",
    "**/app/**/global-error.{js,jsx,ts,tsx}",
    "**/app/**/route.{js,jsx,ts,tsx}",
    // Pages Router (legacy)
    "**/pages/**/*.{js,jsx,ts,tsx}",
    // Root entrypoints
    "**/middleware.{js,ts}",
    "**/instrumentation.{js,ts}",
];

/// `resolve_framework_globs` の auto-detect 対応版。
///
/// 呼び出し側で `framework` が明示指定されていれば従来通り `resolve_framework_globs` に
/// 委譲する。未指定の場合は `dir` 配下の `package.json` を調べ、`next` 依存を持つ
/// workspace ごとに相対化したプリセットを適用する。明示指定が auto detect より常に優先される。
///
/// 自動検出に失敗した場合 (package.json なし、JSON パース失敗、依存不一致) は空 Vec を
/// 返す。debug ログを出さない (副作用最小化のため、検出結果は呼び出し側の review JSON 等で
/// 表現する余地を残す)。
pub(crate) fn resolve_framework_globs_with_auto_detect(
    framework: Option<&str>,
    dir: &str,
) -> Result<Vec<String>> {
    if framework.is_some() {
        return resolve_framework_globs(framework);
    }

    let mut globs = Vec::new();
    for workspace in auto_detect_framework_roots(dir) {
        let Some(prefix) = workspace.to_str() else {
            // glob は UTF-8 文字列なので、表現できない workspace は誤って別パスを除外するより
            // 検出しない側へ倒す。
            continue;
        };
        if prefix.is_empty() {
            globs.extend(
                NEXTJS_PRESET_EXCLUDE_GLOBS
                    .iter()
                    .map(|glob| (*glob).to_string()),
            );
            continue;
        }
        let escaped_prefix = globset::escape(&prefix.replace(std::path::MAIN_SEPARATOR, "/"));
        globs.extend(
            NEXTJS_PRESET_EXCLUDE_GLOBS
                .iter()
                .map(|glob| format!("{escaped_prefix}/{glob}")),
        );
    }
    globs.sort_unstable();
    globs.dedup();
    Ok(globs)
}

/// Next.js プロジェクトとして判定できた workspace の、`dir` からの相対パスを返す。
///
/// 判定: `package.json` の `dependencies` または `devDependencies` に `next` キーが存在
/// すること。`peerDependencies` / `optionalDependencies` は Next.js ライブラリやテスト
/// fixture で誤爆しやすいため対象外。
///
/// ルートとモノレポ配下を有界走査し、有効な `package.json` が無い場合や Next.js 依存が
/// 見つからない場合は `None` を返す。個別 manifest の読み込み・JSON 解析失敗はその候補だけを
/// 無視し、ほかの workspace 候補を引き続き調べる。
const FRAMEWORK_PACKAGE_JSON_MAX_BYTES: u64 = 2 * 1024 * 1024;
const FRAMEWORK_SCAN_MAX_DEPTH: usize = 4;

/// 自動検出で探索しない依存物・生成物ディレクトリ。
///
/// `node_modules` 配下の package.json を見ると、対象プロジェクト自身が Next.js を使って
/// いなくても推移的依存に含まれる `next` を拾う。生成物も同じ manifest を複製しうるため、
/// ワークスペース候補から明示的に外す。
const FRAMEWORK_SCAN_SKIP_DIRS: &[&str] = &[
    "node_modules",
    "vendor",
    "dist",
    "build",
    "target",
    "out",
    ".next",
    "coverage",
];

fn package_json_has_next(path: &std::path::Path) -> bool {
    let Some(text) =
        crate::engine::bounded_read::read_utf8_file_limited(path, FRAMEWORK_PACKAGE_JSON_MAX_BYTES)
    else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    ["dependencies", "devDependencies"].iter().any(|field| {
        value
            .get(field)
            .and_then(|v| v.as_object())
            .is_some_and(|deps| deps.contains_key("next"))
    })
}

fn should_visit_framework_scan_entry(entry: &ignore::DirEntry) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    let Some(file_type) = entry.file_type() else {
        return false;
    };
    if file_type.is_symlink() {
        return false;
    }
    !file_type.is_dir()
        || !FRAMEWORK_SCAN_SKIP_DIRS
            .iter()
            .any(|name| entry.file_name() == *name)
}

pub(crate) fn auto_detect_framework_roots(dir: &str) -> Vec<std::path::PathBuf> {
    let root = std::path::Path::new(dir);
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .follow_links(false)
        .max_depth(Some(FRAMEWORK_SCAN_MAX_DEPTH))
        .filter_entry(should_visit_framework_scan_entry);

    let mut workspaces: Vec<std::path::PathBuf> = builder
        .build()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_type().is_some_and(|kind| kind.is_file())
                && entry.file_name() == "package.json"
                && package_json_has_next(entry.path())
        })
        .filter_map(|entry| {
            entry
                .path()
                .parent()
                .and_then(|parent| parent.strip_prefix(root).ok())
                .map(std::path::Path::to_path_buf)
        })
        .collect();
    workspaces.sort_unstable();
    workspaces.dedup();
    workspaces
}

/// フレームワーク名から対応する除外 glob プリセットを返す。
/// 未知のフレームワーク名はエラー。
///
/// `**/app/X/**` / `**/database/X/**` のような app-prefix 付きパターンには、
/// `**/X/**` という prefix 省略版も自動で追加する。これにより以下が同時にカバーされる:
/// - `--dir <project>/app` のように `app/` 直下を指した場合の fallback
/// - `app/` を別名 (例: `core/`) にリネームしている独自レイアウト
/// - Laravel 配下に複数 module を抱えるモノレポ (`<root>/<sub>/Http/Controllers/...`)
///
/// 過剰除外の懸念: `**/Http/**` の類は Laravel 規約以外でも使われ得るが、
/// 既定除外に `vendor/` / `node_modules/` 等のサードパーティ配下が入っており、
/// なおかつ `--framework laravel` を指定しているのは Laravel プロジェクトのみという
/// 前提なので、実用上の誤マッチはほぼ発生しない。
pub(crate) fn resolve_framework_globs(framework: Option<&str>) -> Result<Vec<String>> {
    match framework {
        None => Ok(Vec::new()),
        Some(name) => match name.to_ascii_lowercase().as_str() {
            "laravel" => {
                let mut globs: Vec<String> =
                    Vec::with_capacity(LARAVEL_PRESET_EXCLUDE_GLOBS.len() * 2);
                for pat in LARAVEL_PRESET_EXCLUDE_GLOBS {
                    globs.push((*pat).to_string());
                    // app/database prefix の省略版を並列で登録 (--dir が app/ 直下の場合の fallback、
                    // および Laravel 標準外レイアウトへの自動対応)
                    if let Some(rest) = pat
                        .strip_prefix("**/app/")
                        .or_else(|| pat.strip_prefix("**/database/"))
                    {
                        globs.push(format!("**/{rest}"));
                    }
                }
                Ok(globs)
            }
            "nextjs" | "next" => {
                // Next.js は `app/` と `pages/` が予約ディレクトリ名で、`src/app/`
                // / `src/pages/` レイアウトも `**/app/**` / `**/pages/**` グロブで
                // そのままカバーされるため prefix 省略形は不要。
                // むしろ `**/pages/**/*.{js,jsx,ts,tsx}` の省略形は
                // `**/*.{js,jsx,ts,tsx}` となり全 TS/JS ファイルを誤除外するので
                // Laravel と異なり省略形を生成しない。
                Ok(NEXTJS_PRESET_EXCLUDE_GLOBS
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect())
            }
            other => Err(AstroError::new(
                ErrorCode::InvalidRequest,
                format!("Unknown framework preset: {other} (supported: laravel, nextjs)"),
            )
            .into()),
        },
    }
}

/// 指定パスが既定除外対象のディレクトリセグメントを含むかを判定する。
pub(crate) fn path_is_default_excluded(path: &str, excludes: &[&str]) -> bool {
    if excludes.is_empty() {
        return false;
    }
    path.split('/').any(|seg| excludes.contains(&seg))
}

/// `diff_files` を dead-code 検出対象に絞り込む共通ヘルパー。
/// `cmd_dead_code` と `cmd_review` の両者から呼び、除外ロジックを一元化する。
///
/// - `excludes`: 既定除外ディレクトリ名 (vendor / tests / build 等、呼び出し側で合成済み)
/// - `combined_exclude_globs`: framework プリセット + ユーザ指定 `--exclude-glob` を合成したパターン列
/// - `glob`: positive glob フィルタ。指定時は whitelist されたもののみ残す。
pub(crate) fn filter_diff_files_for_dead_code(
    canonical_dir: &std::path::Path,
    diff_files: &[crate::models::impact::DiffFile],
    excludes: &[&str],
    combined_exclude_globs: &[&str],
    glob: Option<&str>,
) -> Result<Vec<std::path::PathBuf>> {
    // 除外判定は workspace 相対の new_path で行う。canonical_dir に `test` 等の
    // 親セグメントが含まれているケース (例: `/private/tmp/test/myrepo`) でも、
    // リポ内の `src/foo.rs` のような non-test ファイルを誤って除外しないようにするため。
    let mut files: Vec<std::path::PathBuf> = diff_files
        .iter()
        .filter(|f| f.new_path != "/dev/null")
        // diff の new_path は信頼境界外。絶対パスやトラバーサル成分を含むパスは
        // canonical_dir.join() で workspace 外を指してしまうため、ここで弾く。
        .filter(|f| crate::engine::impact::is_safe_diff_path(&f.new_path))
        .filter(|f| !path_is_default_excluded(&f.new_path, excludes))
        .map(|f| canonical_dir.join(&f.new_path))
        .filter(|p| {
            crate::language::LangId::from_path(camino::Utf8Path::new(p.to_str().unwrap_or("")))
                .is_ok()
        })
        .collect();

    if glob.is_some() || !combined_exclude_globs.is_empty() {
        let mut ob = ignore::overrides::OverrideBuilder::new(canonical_dir);
        if let Some(pattern) = glob {
            ob.add(pattern)?;
        } else {
            ob.add("**/*")?;
        }
        for pat in combined_exclude_globs {
            let negated = if pat.starts_with('!') {
                (*pat).to_string()
            } else {
                format!("!{pat}")
            };
            ob.add(&negated)?;
        }
        let overrides = ob.build()?;
        files.retain(|p| !overrides.matched(p, false).is_ignore());
        // glob が指定されているときは「whitelist に明示マッチ」だけを残す。
        // `Match::None` (どのパターンにもマッチしない) を許可すると、
        // `--glob '**/*.py'` のような絞り込みでも Rust ファイル等が残ってしまう。
        if glob.is_some() {
            files.retain(|p| overrides.matched(p, false).is_whitelist());
        }
    }
    Ok(files)
}

/// PHPUnit 命名規約に合致するシンボルかどうかを判定する。
///
/// PHP プロジェクト (Laravel を含む) ではテストメソッドは `public function testXxx` や
/// `setUp` / `tearDown` / `setUpBeforeClass` / `tearDownAfterClass` 等、PHPUnit が自動で
/// 呼び出す規約的メソッドが大半。識別子レベルの cross-file ref は生じないが dead ではない。
///
/// 同じ規約は JUnit / NUnit / MSTest でも使われるが誤判定を避けるため、本判定は PHP
/// ファイルに限定する。
pub(crate) fn is_phpunit_test_symbol(
    name: &str,
    kind: crate::models::symbol::SymbolKind,
    lang_id: crate::language::LangId,
) -> bool {
    use crate::language::LangId;
    use crate::models::symbol::SymbolKind;
    if lang_id != LangId::Php {
        return false;
    }
    // qualname (`Foo.testBar`) の末尾要素を取る
    let short = name.rsplit_once('.').map(|(_, t)| t).unwrap_or(name);
    match kind {
        SymbolKind::Class => is_phpunit_test_class_name(short),
        SymbolKind::Method | SymbolKind::Function => {
            matches!(
                short,
                "setUp" | "tearDown" | "setUpBeforeClass" | "tearDownAfterClass"
            ) || is_phpunit_test_method_name(short)
        }
        _ => false,
    }
}

/// PHPUnit テストクラス名規約 (`*Test` / `*TestCase` / `*IntegrationTest` / `*FeatureTest`)
/// に合致するか判定する。test_only_symbols 振り分け時に container 名と突き合わせる。
pub(crate) fn is_phpunit_test_class_name(name: &str) -> bool {
    name.ends_with("Test")
        || name.ends_with("TestCase")
        || name.ends_with("IntegrationTest")
        || name.ends_with("FeatureTest")
}

/// PHP シンボルの qualname container が PHPUnit テストクラス規約に合致するか判定する。
///
/// 該当メソッドは同一クラス内の self::/static::/$this-> ヘルパー、または @dataProvider /
/// @depends / #[DataProvider] 経由で reflection 呼び出しされる helper である可能性が高く、
/// test_only_symbols としてレポートしても「テストランナーが内部で使うだけのノイズ」になる
/// ため、test_only からも除外するために使う。
fn has_phpunit_test_container(name: &str, lang: crate::language::LangId) -> bool {
    if lang != crate::language::LangId::Php {
        return false;
    }
    let Some((container, _)) = name.rsplit_once('.') else {
        return false;
    };
    let container_short = container
        .rsplit_once('.')
        .map(|(_, t)| t)
        .unwrap_or(container);
    is_phpunit_test_class_name(container_short)
}

/// `^test[A-Z_]` で始まるメソッド名かどうか (PHPUnit の testXxx 規約)。
pub(crate) fn is_phpunit_test_method_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.len() <= 4 {
        return false;
    }
    if &bytes[..4] != b"test" {
        return false;
    }
    let c = bytes[4];
    c.is_ascii_uppercase() || c == b'_'
}

/// `unittest.TestCase` / `unittest.IsolatedAsyncioTestCase` を直接示す base 名集合。
pub(crate) const PYTHON_UNITTEST_ROOT_BASES: &[&str] = &[
    "TestCase",
    "unittest.TestCase",
    "IsolatedAsyncioTestCase",
    "unittest.IsolatedAsyncioTestCase",
];

/// 同一ファイル内の Python クラスについて、`unittest.TestCase` 系を直接/間接継承する
/// クラス名集合を fixed-point で解決して返す。クロスファイル継承は対象外。
///
/// 例: `class Base(unittest.TestCase): ...` と `class Child(Base): ...` の両方を拾う。
pub(crate) fn collect_python_unittest_classes(
    syms: &[crate::models::symbol::Symbol],
    root: tree_sitter::Node<'_>,
    source: &[u8],
    lang_id: crate::language::LangId,
) -> std::collections::HashSet<String> {
    use crate::models::symbol::SymbolKind;
    let mut unittest_classes: std::collections::HashSet<String> = std::collections::HashSet::new();
    if lang_id != crate::language::LangId::Python {
        return unittest_classes;
    }

    // (クラス名, 解決待ち base 名のリスト) のペアを集める。
    let mut class_bases: Vec<(String, Vec<String>)> = Vec::new();
    for sym in syms {
        if !matches!(sym.kind, SymbolKind::Class) {
            continue;
        }
        let bases = crate::engine::symbols::python_class_base_names(root, source, &sym.range);
        // 直接 root base を継承していれば即座に確定。
        if bases
            .iter()
            .any(|b| PYTHON_UNITTEST_ROOT_BASES.contains(&b.as_str()))
        {
            unittest_classes.insert(sym.name.clone());
            continue;
        }
        // それ以外は候補として保留し、後段で fixed-point 解決する。
        class_bases.push((sym.name.clone(), bases));
    }

    // 同一ファイル内の Base → Child チェーンを fixed-point で広げる。
    loop {
        let mut changed = false;
        let mut idx = 0;
        while idx < class_bases.len() {
            let inherited = class_bases[idx]
                .1
                .iter()
                .any(|b| unittest_classes.contains(b.as_str()));
            if inherited {
                let (name, _) = class_bases.swap_remove(idx);
                unittest_classes.insert(name);
                changed = true;
            } else {
                idx += 1;
            }
        }
        if !changed {
            break;
        }
    }

    unittest_classes
}

/// ファイル名が pytest のモジュール命名規約 (`test_*.py` または `*_test.py`) に
/// 一致するかを判定する。`conftest.py` は別関数で判定する。
pub(crate) fn file_name_is_pytest_module(file_path: Option<&str>) -> bool {
    let Some(path) = file_path else {
        return false;
    };
    let file_name = std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if !file_name.ends_with(".py") {
        return false;
    }
    // conftest.py は別ハンドリング。
    if file_name == "conftest.py" {
        return false;
    }
    if file_name.starts_with("test_") {
        return true;
    }
    // `*_test.py` 規約 (ファイル名が `_test.py` で終わる)。
    let stem = file_name.trim_end_matches(".py");
    stem.ends_with("_test") && stem.len() > "_test".len()
}

/// ファイル名が pytest の `conftest.py` かどうかを判定する。
pub(crate) fn file_name_is_python_conftest(file_path: Option<&str>) -> bool {
    let Some(path) = file_path else {
        return false;
    };
    std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        == Some("conftest.py")
}

/// `unittest` / pytest のテスト規約に該当する Python シンボルかを判定する。
///
/// 対象:
/// - `unittest.TestCase` 直接/間接継承クラス (同一ファイル内のチェーン)
/// - そのクラス配下の `test_*` メソッドおよび `setUp` / `tearDown` /
///   `setUpClass` / `tearDownClass` / `addCleanup` / `addClassCleanup`
/// - `test_*.py` / `*_test.py` のトップレベル `test_*` 関数 (pytest 規約)
/// - `conftest.py` 内のすべての関数 (pytest フィクスチャ規約)
pub(crate) fn is_python_test_symbol(
    name: &str,
    kind: crate::models::symbol::SymbolKind,
    lang_id: crate::language::LangId,
    file_path: Option<&str>,
    container: Option<&str>,
    unittest_classes: &std::collections::HashSet<String>,
) -> bool {
    use crate::language::LangId;
    use crate::models::symbol::SymbolKind;
    if lang_id != LangId::Python {
        return false;
    }

    // qualname (`Foo.test_bar`) の場合は末尾要素を取り出して container を補正する。
    let (short, qual_container) = match name.rsplit_once('.') {
        Some((head, tail)) => (tail, Some(head)),
        None => (name, None),
    };
    let effective_container = container.or(qual_container);

    if matches!(kind, SymbolKind::Class) {
        return unittest_classes.contains(short);
    }

    if !matches!(kind, SymbolKind::Function | SymbolKind::Method) {
        return false;
    }

    // conftest.py 内の関数はすべて pytest 規約で参照されうる。
    if file_name_is_python_conftest(file_path) && effective_container.is_none() {
        return true;
    }

    // `test_*.py` / `*_test.py` のトップレベル `test_*` 関数は pytest が discover する。
    if file_name_is_pytest_module(file_path)
        && effective_container.is_none()
        && short.starts_with("test_")
    {
        return true;
    }

    // unittest.TestCase 派生クラス配下のメソッド。
    if let Some(class_name) = effective_container
        && unittest_classes.contains(class_name)
    {
        return short.starts_with("test_")
            || matches!(
                short,
                "setUp"
                    | "tearDown"
                    | "setUpClass"
                    | "tearDownClass"
                    | "asyncSetUp"
                    | "asyncTearDown"
                    | "addCleanup"
                    | "addClassCleanup"
                    | "addAsyncCleanup"
            );
    }

    false
}

/// `dead-code` CLI の入力一式。
///
/// 個数の多い同型引数を位置で渡すと、フラグ追加時に CLI 層と実行層の対応を崩しやすい。
/// 他の複合コマンド (`review` / `context` / `impact`) と同じく名前付きの options に閉じる。
pub struct CmdDeadCodeOpts<'a> {
    pub dir: &'a str,
    pub glob: Option<&'a str>,
    pub diff: Option<&'a str>,
    pub diff_file: Option<&'a str>,
    pub git: bool,
    /// `--base` の指定。省略なら `None` (既定の HEAD を使い、HEAD が unborn なら skip)。
    pub base: Option<&'a str>,
    pub staged: bool,
    pub include_vendor: bool,
    pub include_tests: bool,
    pub include_build: bool,
    pub framework: Option<&'a str>,
    pub extra_exclude_dirs: &'a [String],
    pub extra_exclude_globs: &'a [String],
    pub output: OutputOptions,
    pub dead_scope: crate::cli::DeadScope,
    /// 生成物も dead の候補にするか (グローバル `--include-generated` / config の
    /// `skip_generated = false`)。参照の集計には常に生成物を含める。
    pub include_generated: bool,
}

struct ResolvedDeadCodeDiff {
    input: Option<String>,
    files: Option<Vec<crate::models::impact::DiffFile>>,
    truncations: Vec<crate::models::truncation::TruncationInfo>,
}

pub fn cmd_dead_code(opts: &CmdDeadCodeOpts<'_>) -> Result<()> {
    cmd_dead_code_with_swift_protocols(opts, &SwiftProtocolMethods::default())
}

/// 検証済みの外部 Swift protocol 要件を追加して dead-code を実行する。
pub fn cmd_dead_code_with_swift_protocols(
    opts: &CmdDeadCodeOpts<'_>,
    swift_protocols: &SwiftProtocolMethods,
) -> Result<()> {
    let dir = opts.dir;
    let glob = opts.glob;
    let diff = opts.diff;
    let diff_file = opts.diff_file;
    let git = opts.git;
    let base = opts.base;
    let staged = opts.staged;
    let include_vendor = opts.include_vendor;
    let include_tests = opts.include_tests;
    let include_build = opts.include_build;
    let framework = opts.framework;
    let extra_exclude_dirs = opts.extra_exclude_dirs;
    let extra_exclude_globs = opts.extra_exclude_globs;
    let output = opts.output;
    let dead_scope = opts.dead_scope;
    let include_generated = opts.include_generated;

    let canonical_dir = std::fs::canonicalize(dir)?;
    if !canonical_dir.is_dir() {
        return Err(
            AstroError::new(ErrorCode::InvalidRequest, format!("Not a directory: {dir}")).into(),
        );
    }

    let default_excludes = resolve_dead_code_excludes(include_vendor, include_tests, include_build);
    let mut excludes: Vec<&str> = default_excludes.to_vec();
    for name in extra_exclude_dirs {
        excludes.push(name.as_str());
    }

    // glob 除外: フレームワークプリセット + ユーザ指定
    // 未指定時は package.json から next 依存を検出して nextjs プリセットを自動適用する。
    let framework_globs = resolve_framework_globs_with_auto_detect(framework, dir)?;
    let mut combined_globs: Vec<&str> = framework_globs.iter().map(String::as_str).collect();
    for pat in extra_exclude_globs {
        combined_globs.push(pat.as_str());
    }

    // diff 指定があれば diff 関連ファイルのみ、なければプロジェクト全体。
    // diff_input / diff_files は touched-symbols filter でも使うため、ここで一度だけ
    // 取得・parse して再利用する (旧実装は run_git_diff + parse_unified_diff を 2 回呼んでおり、
    // --staged 実行中の git add で 2 つの diff が乖離する競合状態があった)。
    // NotRequested (diff/diff_file/git いずれも未指定) は全体走査に振り分ける。
    let mut pinned_base = None;
    let resolved_diff = match resolve_diff_source(dir, diff, diff_file, git, base, staged)? {
        DiffSourceResolution::Diff {
            diff,
            truncations,
            base_commit,
        } => {
            pinned_base = base_commit;
            if diff.trim().is_empty() {
                // 未追跡の巨大ファイルを除外した結果 diff が空になった場合も、
                // 何を対象外にしたかを報告する (空結果と打ち切りを混同させない)。
                let result = DeadCodeResult {
                    dir: canonical_dir.to_string_lossy().to_string(),
                    scanned_files: 0,
                    dead_symbols: Vec::new(),
                    test_only_symbols: Vec::new(),
                    unverified_symbols: Vec::new(),
                    skipped: None,
                    truncations,
                    generated_candidates_skipped: None,
                };
                print!("{}", serialize_cli_document(&result, output)?);
                return Ok(());
            }

            let parsed = crate::engine::diff::parse_unified_diff(&diff);
            ResolvedDeadCodeDiff {
                input: Some(diff),
                files: Some(parsed),
                truncations,
            }
        }
        DiffSourceResolution::Skipped(skip) => {
            // git 管理外: 空の dead_symbols + skipped で exit 0。
            let result = DeadCodeResult {
                dir: canonical_dir.to_string_lossy().to_string(),
                scanned_files: 0,
                dead_symbols: Vec::new(),
                test_only_symbols: Vec::new(),
                unverified_symbols: Vec::new(),
                skipped: Some(skip),
                truncations: Vec::new(),
                generated_candidates_skipped: None,
            };
            print!("{}", serialize_cli_document(&result, output)?);
            return Ok(());
        }
        DiffSourceResolution::NotRequested => ResolvedDeadCodeDiff {
            input: None,
            files: None,
            truncations: Vec::new(),
        },
    };

    // 候補・準拠・参照を同じ index の版で読む。公開先の dir は元のパスを保つ。
    let output_dir = canonical_dir.clone();
    let snapshot = if matches!(
        super::git_input::diff_new_side(diff, diff_file, git, staged),
        crate::models::impact::DiffNewSide::Index
    ) && resolved_diff.files.as_ref().is_some_and(|files| {
        files
            .iter()
            .any(|file| file.new_path.to_ascii_lowercase().ends_with(".swift"))
    }) {
        Some(super::review::prepare_dead_code_snapshot(
            dir,
            pinned_base.as_deref().unwrap_or("HEAD"),
            resolved_diff.input.as_deref().unwrap_or(""),
        )?)
    } else {
        None
    };
    let dir = snapshot
        .as_ref()
        .map(|snapshot| snapshot.dir())
        .transpose()?
        .unwrap_or(dir);
    let canonical_dir = std::fs::canonicalize(dir)?;

    // 候補ファイルの収集。生成物として候補から外したファイルは申告用に控える
    // (ディレクトリ走査では名前 / ヘッダマーカーで走査の段階から外れる)。
    let (files, mut generated_skipped): (Vec<std::path::PathBuf>, Vec<String>) =
        if let Some(diff_files) = resolved_diff.files.as_ref() {
            let files = filter_diff_files_for_dead_code(
                &canonical_dir,
                diff_files,
                &excludes,
                &combined_globs,
                glob,
            )?;
            (files, Vec::new())
        } else {
            let collection = crate::engine::refs::collect_files_scan_with_excludes(
                &canonical_dir,
                glob,
                &excludes,
                &combined_globs,
                crate::engine::refs::FileScanOptions { include_generated },
            )?;
            let generated_skipped = collection
                .skipped_generated_relative(&canonical_dir)
                .iter()
                .map(|path| normalize_workspace_separators(path))
                .collect();
            (collection.files, generated_skipped)
        };

    let scanned_files = files.len();
    // 「ソースだが解析できなかったファイル」は参照集計と同じ走査集合から申告する
    // (`detection.unanalyzable`)。参照を数えられないまま dead と断定すると、`.vue` の
    // `<script>` からしか使われていない TS 関数のように**生きているシンボルを dead と
    // 報告する** (最悪方向の誤り)。候補の絞り込み (`--glob` / `--git`) で申告を狭めると、
    // 数えていない範囲が申告から消える。
    let detection =
        detect_dead_symbols_with_swift_protocols(dir, &files, include_generated, swift_protocols);
    generated_skipped.extend(detection.generated_candidates_skipped);
    let dead_symbols = detection.dead;
    let test_only_symbols = detection.test_only;

    // dead-scope=touched-symbols: --git/--diff 指定時のみ意味を持つ。
    // diff の追加行情報が必要なので、has_diff のときだけ適用する。
    let dead_symbols = if matches!(dead_scope, crate::cli::DeadScope::TouchedSymbols)
        && let (Some(diff_input), Some(diff_files)) =
            (resolved_diff.input.as_deref(), resolved_diff.files.as_ref())
    {
        filter_dead_by_touched_symbols(dir, dead_symbols, diff_input, diff_files)
    } else {
        dead_symbols
    };

    let mut truncations = resolved_diff.truncations;
    truncations.extend(detection.unanalyzable);
    // 参照を数えられなかったファイルは、dead を保留したかに関わらず申告する
    // (「全部数えた」と読めないように)。
    truncations.extend(detection.uncounted);

    let result = DeadCodeResult {
        dir: output_dir.to_string_lossy().to_string(),
        scanned_files,
        dead_symbols,
        test_only_symbols,
        unverified_symbols: detection.unverified,
        skipped: None,
        truncations,
        generated_candidates_skipped: crate::engine::refs::skipped_files_from_relative(
            generated_skipped,
        ),
    };

    let text = serialize_cli_document(&result, output)?;
    info!(
        command = "dead-code",
        dir = dir,
        scanned_files = scanned_files,
        dead_count = result.dead_symbols.len(),
        "command completed"
    );
    print!("{text}");
    Ok(())
}

#[cfg(test)]
mod uncounted_tests {
    use super::*;

    fn sym(name: &str) -> DeadSymbol {
        DeadSymbol {
            name: name.to_string(),
            kind: "function".to_string(),
            file: "a.ts".to_string(),
            line: None,
        }
    }

    fn names(symbols: &[DeadSymbol]) -> Vec<&str> {
        symbols.iter().map(|s| s.name.as_str()).collect()
    }

    /// 数えられなかった参照がありうる候補は断定しない。test_only は production の参照が
    /// 無いことの断定なので production 側だけ、dead は両方を見る。
    #[test]
    fn classified_dead_withholds_only_what_unobserved_files_could_change() {
        let mut classified = ClassifiedDead::default();
        // (名前, test の参照あり, (production の未観測, test の未観測))
        for (name, has_test_refs, unobserved) in [
            ("dead_clean", false, (false, false)),
            ("dead_prod_unobserved", false, (true, false)),
            ("dead_test_unobserved", false, (false, true)),
            ("test_only_clean", true, (false, false)),
            ("test_only_prod_unobserved", true, (true, false)),
            ("test_only_test_unobserved", true, (false, true)),
        ] {
            classified.push(sym(name), has_test_refs, unobserved);
        }
        assert_eq!(names(&classified.dead), vec!["dead_clean"]);
        assert_eq!(
            names(&classified.test_only),
            vec!["test_only_clean", "test_only_test_unobserved"]
        );
        assert_eq!(
            names(&classified.unverified),
            vec![
                "dead_prod_unobserved",
                "dead_test_unobserved",
                "test_only_prod_unobserved"
            ]
        );
    }

    /// 参照しうる名前が分かるファイルはその名前だけ、中身を確かめられないファイルは
    /// 全部の名前を未観測にする。production / test を分けて持つ。
    #[test]
    fn unobserved_references_follow_possible_names() {
        let names: Vec<String> = ["alpha", "beta", "gamma"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let file = |path: &str, possible: Option<Vec<usize>>, is_test: bool| {
            crate::engine::refs::UncountedFile {
                path: std::path::PathBuf::from(path),
                cause: "x".to_string(),
                possible_names: possible,
                is_test,
            }
        };
        let unobserved = UnobservedReferences::new(
            &names,
            &[
                file("big.ts", Some(vec![0]), false),
                file("t.ts", Some(vec![1]), true),
            ],
        );
        assert_eq!(unobserved.may_be_referenced(&["alpha"]), (true, false));
        assert_eq!(unobserved.may_be_referenced(&["beta"]), (false, true));
        assert_eq!(unobserved.may_be_referenced(&["gamma"]), (false, false));
        // liveness 補助名 (列挙子など) のどれかが未観測でも断定しない
        assert_eq!(
            unobserved.may_be_referenced(&["gamma", "alpha"]),
            (true, false)
        );

        let unreadable = UnobservedReferences::new(&names, &[file("locked.ts", None, false)]);
        assert_eq!(unreadable.may_be_referenced(&["gamma"]), (true, false));
        // 対照: 失敗が無ければ何も未観測にしない
        let clean = UnobservedReferences::new(&names, &[]);
        assert_eq!(clean.may_be_referenced(&["alpha"]), (false, false));
    }
}

#[cfg(test)]
mod candidate_tests {
    use super::*;
    use crate::language::LangId;

    /// 変更前の C/C++ liveness 収集 (候補抽出とは別に読み直して parse していた)。
    fn reference_cpp_liveness(dir: &str, rel: &str, lang: LangId) -> Vec<(String, Vec<String>)> {
        let full = std::path::Path::new(dir).join(rel);
        let Some(full_str) = full.to_str() else {
            return Vec::new();
        };
        let utf8 = camino::Utf8Path::new(full_str);
        let Ok(source) = parser::read_file(utf8) else {
            return Vec::new();
        };
        let Ok(tree) = parser::parse_source(&source, lang) else {
            return Vec::new();
        };
        crate::engine::symbols::collect_cpp_dead_liveness_aliases(tree.root_node(), &source, lang)
    }

    type ReferenceCandidates = (
        Vec<(String, String, String, LangId)>,
        Vec<(String, String, Vec<String>, LangId)>,
        Vec<String>,
    );

    /// 変更前の候補収集 (64bfb37)。直列に処理し、複雑度も計算する公開 API 用の抽出を使い、
    /// C/C++ は liveness のために読み直して parse していた。
    fn reference_collect(
        dir: &str,
        canonical_dir: &std::path::Path,
        files: &[std::path::PathBuf],
        include_generated: bool,
    ) -> ReferenceCandidates {
        let gitattrs = crate::engine::gitattributes::GitAttributes::load(canonical_dir);
        let mut all_syms = Vec::new();
        let mut liveness_raw = Vec::new();
        let mut generated_skipped = Vec::new();
        for path in files {
            let Ok(canonical_path) = std::fs::canonicalize(path) else {
                continue;
            };
            let rel = match canonical_path.strip_prefix(canonical_dir) {
                Ok(p) => normalize_workspace_separators(&p.to_string_lossy()),
                Err(_) => continue,
            };
            if !include_generated
                && (gitattrs.is_generated(&rel)
                    || crate::engine::generated::is_auto_generated(&canonical_path))
            {
                generated_skipped.push(rel);
                continue;
            }
            if let Some((lang, syms)) =
                super::super::api_changes::extract_exported_symbols_from_file_inner_with_lang(
                    dir, &rel, true, true,
                )
            {
                for (name, kind, _sig) in syms {
                    all_syms.push((name, kind, rel.clone(), lang));
                }
                if matches!(lang, LangId::C | LangId::Cpp) {
                    for (sym, extras) in reference_cpp_liveness(dir, &rel, lang) {
                        liveness_raw.push((rel.clone(), sym, extras, lang));
                    }
                }
            }
        }
        (all_syms, liveness_raw, generated_skipped)
    }

    fn list_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read_dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                list_files(&path, out);
            } else {
                out.push(path);
            }
        }
    }

    /// 多言語の候補を持つリポジトリ。tests/fixtures の各言語のサンプルに、C/C++ の liveness・
    /// 生成物 (ヘッダのマーカーと .gitattributes)・lexer-only・shebang のファイルを足す。
    fn write_fixture_repo() -> tempfile::TempDir {
        let repo = tempfile::tempdir().expect("tempdir");
        let src = repo.path().join("src");
        std::fs::create_dir_all(&src).expect("mkdir");
        let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        for entry in std::fs::read_dir(&fixtures).expect("fixtures") {
            let path = entry.expect("entry").path();
            if path.is_file() {
                std::fs::copy(&path, src.join(path.file_name().unwrap())).expect("copy");
            }
        }
        let extra = [
            (
                "src/lib.rs",
                "pub fn top() -> u32 { 1 }\npub struct Holder;\nimpl Holder {\n    pub fn get(&self) -> u32 { if true { 2 } else { 3 } }\n}\nimpl Default for Holder {\n    fn default() -> Self { Holder }\n}\npub trait Shape {\n    fn area(&self) -> f64;\n}\npub const LIMIT: u32 = 3;\n",
            ),
            (
                "src/enums.h",
                "typedef enum { RED, GREEN } color_t;\nenum mode { MODE_A, MODE_B };\ntypedef struct node { int v; } node_t;\nint api(void);\nint api(void) { return RED; }\n",
            ),
            (
                "src/widget.tsx",
                "export function Widget() { return <div />; }\nexport class Store {\n  load() { return 1; }\n}\n",
            ),
            (
                "src/util.js",
                "export function helper() { return 1; }\nexport const value = 2;\n",
            ),
            (
                "src/gen.go",
                "// Code generated by tool. DO NOT EDIT.\npackage gen\n\nfunc Generated() {}\n",
            ),
            ("src/marked.py", "def marked():\n    return 1\n"),
            ("src/notes.txt", "not a source file\n"),
            (
                "bin/tool",
                "#!/usr/bin/env python3\ndef tool_main():\n    return 0\n",
            ),
            (".gitattributes", "src/marked.py linguist-generated\n"),
        ];
        for (rel, body) in extra {
            let path = repo.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");
            std::fs::write(path, body).expect("write");
        }
        repo
    }

    /// 並列化した候補収集は、変更前の直列の収集と同じ候補・liveness・生成物の申告を同じ順序で
    /// 返す (C/C++ の liveness を候補抽出の構文木から集めても、複雑度を計算しなくても同じ)。
    /// 入力の順を逆にしても、その順のまま組み立てる。
    #[test]
    fn candidate_collection_matches_sequential_reference() {
        let repo = write_fixture_repo();
        let dir = repo.path().to_str().expect("utf-8 path");
        let canonical_dir = std::fs::canonicalize(repo.path()).expect("canonicalize");
        let outside = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            outside.path().join("x.ts"),
            "export function outside() {}\n",
        )
        .expect("write");

        let mut files = Vec::new();
        list_files(repo.path(), &mut files);
        files.sort();
        files.reverse();
        files.push(repo.path().join("src/missing.ts"));
        files.push(outside.path().join("x.ts"));

        for include_generated in [false, true] {
            let actual =
                collect_dead_code_candidates(dir, &canonical_dir, &files, include_generated);
            let (all_syms, liveness_raw, generated_skipped) =
                reference_collect(dir, &canonical_dir, &files, include_generated);
            assert_eq!(
                actual.all_syms, all_syms,
                "include_generated={include_generated}"
            );
            assert_eq!(
                actual.liveness_raw, liveness_raw,
                "include_generated={include_generated}"
            );
            assert_eq!(
                actual.generated_skipped, generated_skipped,
                "include_generated={include_generated}"
            );

            // 空振り防止: 比べた中身に各経路が実際に含まれている
            let langs: HashSet<LangId> = all_syms.iter().map(|(_, _, _, lang)| *lang).collect();
            assert!(langs.len() >= 12, "{langs:?}");
            assert!(langs.contains(&LangId::Xojo), "lexer-only: {langs:?}");
            assert!(
                all_syms
                    .iter()
                    .any(|(name, _, file, _)| name == "tool_main" && file == "bin/tool"),
                "shebang で判定するファイル"
            );
            assert!(!liveness_raw.is_empty(), "C/C++ の liveness 補助情報");
            if include_generated {
                assert!(generated_skipped.is_empty());
            } else {
                assert_eq!(generated_skipped, vec!["src/marked.py", "src/gen.go"]);
            }
        }

        // ファイル単位でも、旧関数 (公開 API 用の抽出を dead-code の設定で呼ぶもの) と同じ候補
        let mut compared = 0;
        for path in &files {
            let Ok(canonical) = std::fs::canonicalize(path) else {
                continue;
            };
            let Ok(rel) = canonical.strip_prefix(&canonical_dir) else {
                continue;
            };
            let rel = normalize_workspace_separators(&rel.to_string_lossy());
            let expected = extract_exported_symbols_from_file_inner(dir, &rel, true, true);
            compared += usize::from(expected.is_some());
            assert_eq!(
                extract_dead_code_candidates_from_file(dir, &rel),
                expected,
                "{rel}"
            );
        }
        assert!(compared >= 20, "{compared}");
    }

    /// dead-code の候補抽出に使う `extract_symbols_without_complexity` は、`extract_symbols` と
    /// 同じシンボルを複雑度だけ計算せずに返す。
    #[test]
    fn symbols_without_complexity_differ_only_in_complexity() {
        let repo = write_fixture_repo();
        let mut files = Vec::new();
        list_files(repo.path(), &mut files);
        files.sort();
        let mut functions_with_complexity = 0;
        for path in files {
            let utf8 = camino::Utf8Path::new(path.to_str().expect("utf-8 path"));
            let source = parser::read_file(utf8).expect("read");
            let Ok(lang) = parser::detect_lang(utf8, &source) else {
                continue;
            };
            if lang.is_lexer_only() {
                continue;
            }
            let tree = parser::parse_source(&source, lang).expect("parse");
            let root = tree.root_node();
            let mut with = crate::engine::symbols::extract_symbols(root, &source, lang).unwrap();
            let without =
                crate::engine::symbols::extract_symbols_without_complexity(root, &source, lang)
                    .unwrap();
            functions_with_complexity += with.iter().filter(|s| s.complexity.is_some()).count();
            assert!(without.iter().all(|s| s.complexity.is_none()), "{path:?}");
            for symbol in &mut with {
                symbol.complexity = None;
            }
            assert_eq!(format!("{with:?}"), format!("{without:?}"), "{path:?}");
        }
        assert!(
            functions_with_complexity > 0,
            "複雑度を持つ関数を含む入力で比べる"
        );
    }
}
