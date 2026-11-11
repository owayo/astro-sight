//! diff の前処理。解析対象の絞り込みと、ファイル単位の事実 (exported / callees / export surface) の先取り抽出を行う。

use super::*;

/// 属性を解決できずに API 差分の対象に残したファイルと原因。
pub(crate) type GeneratedUndetermined = (
    std::path::PathBuf,
    crate::engine::gitattributes::AttributeLookupError,
);

/// API 差分検出から除外すべき diff ファイルかを判定する。
///
/// - 信頼境界外のトラバーサルパスを `dir.join()` で読まないよう、絶対パスや `..` を
///   含むパスは拒否する。
/// - 生成物は refs の走査と同じ判定 (`.gitattributes` の `linguist-generated` → 名前 →
///   ヘッダー。[`crate::engine::generated::judge`]) で対象外にする。新旧のどちらかが生成物なら
///   外す。旧側は作業ツリーに無いので、属性と名前だけで見る。`generated` は属性の照会
///   (`canonical_dir` が無ければ判定しない)。属性を解決できなかった側は外さず、
///   `undetermined` に積む。
pub(crate) fn should_skip_diff_file(
    df: &crate::models::impact::DiffFile,
    generated: Option<&mut crate::engine::gitattributes::AttributeLookup<'_>>,
    canonical_dir: Option<&std::path::Path>,
    undetermined: &mut Vec<GeneratedUndetermined>,
) -> bool {
    use crate::engine::generated::{GeneratedVerdict, judge};

    if df.new_path != "/dev/null" && !crate::engine::impact::is_safe_diff_path(&df.new_path) {
        return true;
    }
    if df.old_path != "/dev/null" && !crate::engine::impact::is_safe_diff_path(&df.old_path) {
        return true;
    }
    let Some(root) = canonical_dir else {
        return false;
    };
    if df.new_path != "/dev/null" {
        let full = root.join(&df.new_path);
        // symlink 経由で workspace 外を指すファイルを on-disk read しないよう、
        // canonicalize して root 配下にあることを fail-closed で確認する。
        // is_safe_diff_path は文字列レベルの check のみで、`evil.rs ->
        // /etc/passwd` のような symlink を検出できないため、実ファイルを読む直前に
        // canonical 境界判定を入れる (canonicalize 失敗時も skip 側へ倒す)。
        match std::fs::canonicalize(&full) {
            Ok(canonical) if canonical.starts_with(root) => {}
            _ => return true,
        }
    }
    let Some(lookup) = generated else {
        return false;
    };
    let mut sides = vec![(df.new_path.as_str(), true)];
    if df.old_path != df.new_path {
        sides.push((df.old_path.as_str(), false));
    }
    let mut side_undetermined = Vec::new();
    for (path, worktree) in sides {
        if path == "/dev/null" {
            continue;
        }
        let full = root.join(path);
        let header = || worktree && crate::engine::generated::is_auto_generated(&full);
        match judge(lookup.linguist_generated(&full), &full, header) {
            GeneratedVerdict::Generated(_) => return true,
            GeneratedVerdict::NotGenerated => {}
            GeneratedVerdict::Undetermined(error) => side_undetermined.push((full, error)),
        }
    }
    undetermined.extend(side_undetermined);
    false
}

/// `detect_api_changes` の各バケット。`reconcile_with_moves` /
/// `partition_removed_dead_candidates` で最終分類する前の中間状態。
#[derive(Default)]
pub(crate) struct ApiChangeBuckets {
    pub(crate) added_reference_candidates: Vec<ApiSymbolCandidate>,
    pub(crate) added: Vec<ApiSymbolCandidate>,
    pub(crate) removed: Vec<ApiSymbolCandidate>,
    pub(crate) uncertain_removals: Vec<crate::models::review::UncertainApiRemoval>,
    pub(crate) parse_truncations: Vec<crate::models::truncation::TruncationInfo>,
    pub(crate) modified: Vec<ApiSymbolChange>,
    pub(crate) modified_closed_in_diff: Vec<ApiSymbolChange>,
    pub(crate) const_value_changes: Vec<ApiSymbolChange>,
    pub(crate) unchanged_static_consts: Vec<ApiSymbol>,
    pub(crate) type_annotation_changes: Vec<crate::models::review::TypeAnnotationChange>,
    pub(crate) unchanged_type_annotations: Vec<ApiSymbol>,
    pub(crate) callback_body_changes: Vec<crate::models::review::CallbackBodyChange>,
    pub(crate) compatible_modified: Vec<CompatibleApiModification>,
    pub(crate) all_new_candidates: Vec<ApiSymbolCandidate>,
    pub(crate) property_to_field: Vec<PropertyToFieldChange>,
    /// 新しいファイルの `pub use` で同じ名前を再公開したので削除にしなかった Rust の旧い宣言と、
    /// 再公開したファイル (新しいパス)。再公開の先の宣言と組にして比べる (Issue #281)。
    pub(crate) reexported_removals: Vec<(ApiSymbolCandidate, String)>,
    /// 有る設定の一部から消えた Rust の項目 (`#[cfg]` を足した・狭めた。Issue #291) と旧新の条件。
    /// 項目は残るので移動の相殺にかけず、参照の有無で `removed` と `removed_dead` に分ける。
    pub(crate) cfg_removed: Vec<(ApiSymbolCandidate, crate::models::review::CfgAvailability)>,
    /// 無かった設定に増えた Rust の項目 (`#[cfg]` を外した・広げた。Issue #291) と旧新の条件。
    /// 追加と同じ絞り込みを通したもの。
    pub(crate) cfg_added: Vec<(ApiSymbolCandidate, crate::models::review::CfgAvailability)>,
    /// 再公開した名前の削除 (`pub use` を消した・公開範囲を狭めた。Issue #318) と、旧版で名前の指して
    /// いた宣言。移動の相殺にかけない。
    pub(crate) reexport_removed: Vec<(
        ApiSymbolCandidate,
        Vec<super::rust_reexport_names::DeclarationId>,
    )>,
    /// 新しく再公開した名前 (Issue #306) と、新版で名前の指す宣言。追加と同じ絞り込みを通したもの。
    /// 移動の相殺にかけない。
    pub(crate) reexport_added: Vec<(
        ApiSymbolCandidate,
        Vec<super::rust_reexport_names::DeclarationId>,
    )>,
}

/// diff 全体で不変な入力。ファイル単位の分類関数が共通で参照する。
///
/// 可変状態の `DetectionState` と型で分けることで、「読むだけの入力」と
/// 「書き換える先」を取り違えてもコンパイルが通らないようにする。
pub(crate) struct DetectionInputs<'a> {
    pub(crate) dir: &'a str,
    pub(crate) base: &'a str,
    pub(crate) diff_files: &'a [crate::models::impact::DiffFile],
    pub(crate) diff_new_paths: &'a HashSet<String>,
    pub(crate) ref_index: &'a ApiRefIndex,
    pub(crate) old_ref_index: &'a std::cell::OnceCell<OldRefIndex>,
    pub(crate) index_names: &'a HashSet<String>,
    pub(crate) prepared: &'a [PreparedDiffFile],
    /// base リビジョンの blob の読み手 (常駐 `git cat-file --batch`)。旧版を読む経路は
    /// すべてこれを通し、ファイルやシンボルごとに `git show` を起動しない。
    pub(crate) base_blobs: &'a crate::commands::git_input::GitBlobBatch,
    /// TS の `stripInternal` の設定 (Issue #274)。公開面の抽出と同じ設定で、互換判定器にも
    /// 宣言ファイルから消える型のメンバーを除いたソースを渡す。
    pub(crate) strip_internal: &'a super::ts_strip_internal::StripInternal,
    /// 差分の中の package の Rust の lib crate (Issue #300)。束縛が変わった package では、契約が
    /// 名指す名前をほかのファイルまで版ごとに解く。
    pub(crate) rust_crates: &'a super::RustCrates<'a>,
    /// module の宣言の条件が変わった module の項目の比べ直し (Issue #305)。比べ直した項目は、
    /// ファイルごとの有る設定の比較と削除・追加の候補から外す。
    pub(crate) module_availability: &'a super::rust_module_availability::ModuleAvailability,
    /// 変わった `use` が再公開する名前の比べ直し (Issue #306・#318)。
    pub(crate) reexport_names: &'a super::rust_reexport_names::ReexportNames,
    /// 項目の有る設定の比較に、ファイルを置く module の置き場の条件を足す (Issue #316)。
    pub(crate) placement_availability:
        &'a super::rust_placement_availability::PlacementAvailability<'a, 'a>,
}

/// 分類の過程で更新される可変状態 (結果バケットと、ファイル単位のメモ化キャッシュ)。
pub(crate) struct DetectionState<'a> {
    pub(crate) buckets: &'a mut ApiChangeBuckets,
    /// base/worktree を同じライフタイムへ閉じた Rust 公開面解析コンテキスト。
    pub(crate) rust_public: &'a mut RustPublicApiContext,
    pub(crate) closure_caches: &'a mut crate::commands::api_changes::ref_index::ApiClosureCaches,
}

/// 新規ファイル 1 件について Phase 0 で先取り済みの事実。
pub(crate) struct AddedFileFacts<'a> {
    pub(crate) new_syms: &'a [(String, String, String)],
    pub(crate) in_file_callees: &'a HashSet<String>,
    /// Rust のとき、`new_syms` と同じ順の宣言の文脈 (Issue #282)。
    pub(crate) rust_declarations: Option<&'a [super::RustDeclarationContext]>,
}

/// 変更ファイル 1 件について Phase 0 で先取り済みの事実。
pub(crate) struct ModifiedFileFacts<'a> {
    pub(crate) type_dependencies: &'a super::ts_signature::DependencyChanges,
    pub(crate) unparsed_declarations:
        &'a [crate::engine::bash_parse_recovery::UnparsedBashDeclaration],
    pub(crate) old_syms: &'a [(String, String, String)],
    pub(crate) new_syms: &'a [(String, String, String)],
    pub(crate) parameter_pattern_only_names: &'a HashSet<String>,
    /// Rust の関数に `const` を足しただけと証明できた qualname。
    pub(crate) rust_const_additions: &'a HashSet<String>,
    pub(crate) in_file_callees: &'a HashSet<String>,
    pub(crate) new_export_surface_names: &'a HashSet<String>,
    /// Rust のとき、旧新の公開シンボルの宣言の文脈 (`old_syms`・`new_syms` と同じ順)。
    pub(crate) rust_declarations: Option<&'a super::RustDeclarationPair>,
}

/// シグネチャ変更 1 件を分類するための現場情報。
pub(crate) struct SignatureChangeSite<'a> {
    pub(crate) change: ApiSymbolChange,
    /// 旧版を読むパス (base 側)。rename なら旧パス、移動と同じ差分でシグネチャの
    /// 変わった宣言 (型のメンバー・自由関数・定数) なら移動元。
    pub(crate) old_path: &'a str,
    /// 新版を読むパス (作業ツリー側)。呼び出し側の追随を確かめる定義もここで探す。
    pub(crate) new_path: &'a str,
    /// 移動と同じ差分でシグネチャの変わった宣言なら移動元 (`old_path` と同じ)。
    /// 移動先の行のうち移動元にもある行を、呼び出し側の追随の証拠にしない。
    pub(crate) moved_from: Option<&'a str>,
    pub(crate) kind: &'a str,
    pub(crate) old_sig: &'a str,
    pub(crate) new_sig: &'a str,
    pub(crate) lang_id: Option<crate::language::LangId>,
    /// 同一ファイル内でしか使われておらず、cross-file 参照が 1 件も無い。
    ///
    /// 通常はこの時点で api.mod から落とすが、Python の TypedDict `total=` 変更候補だけは
    /// 契約変更の 3 値判定に必ず通す必要があるため落とさずここまで運び、
    /// **種別を確定できたときだけ**残して他は従来どおり捨てる
    /// (Issue 2026-08-18-python-typeddict-contract-change-classification)。
    /// 証明できないケースまで残すと、`class X(Base, total=False)` のような無関係な
    /// Python class を新たに blocking にしてしまうため。
    pub(crate) internally_closed: bool,
}

/// Phase 0 で抽出した diff ファイルごとの exported シンボル。
/// 抽出は git show / parse を伴うため 1 回だけ行い、name 収集と process_* で共有する。
///
/// 差分ファイルのほとんどは `Modified` なので、大きい variant の値を Box にしても間接参照が
/// 増えるだけで、ほかの variant の分の領域はほとんど節約できない。
#[allow(clippy::large_enum_variant)]
pub(crate) enum PreparedDiffFile {
    /// `should_skip_diff_file` で除外されたファイル。
    Skip,
    /// 新規ファイル (`old_path == "/dev/null"`)。
    Added {
        new_syms: Option<Vec<(String, String, String)>>,
        in_file_callees: std::collections::HashSet<String>,
        /// Rust のとき、`new_syms` と同じ順の宣言の文脈 (Issue #282)。
        rust_declarations: Option<Vec<super::RustDeclarationContext>>,
    },
    /// 削除ファイル (`new_path == "/dev/null"`)、または対応言語でないファイルへの rename
    /// (新側に API 面が無い)。削除 bash 関数の残存参照判定に使う名前を ApiRefIndex へ
    /// 入れるため、旧版の exported シンボルを Phase 0 で抽出しておく。
    Deleted {
        old_syms: Option<Vec<(String, String, String)>>,
        /// 指定 base に旧ソースが無い、または diff と照合できない。
        old_source_unverified: bool,
        /// Rust のとき、`old_syms` と同じ順の宣言の文脈 (Issue #282)。
        rust_declarations: Option<Vec<super::RustDeclarationContext>>,
    },
    /// 通常の modified ファイル。
    Modified {
        type_dependencies: Box<super::ts_signature::DependencyChanges>,
        unparsed_declarations: Vec<crate::engine::bash_parse_recovery::UnparsedBashDeclaration>,
        old_syms: Option<Vec<(String, String, String)>>,
        new_syms: Option<Vec<(String, String, String)>>,
        parameter_pattern_only_names: HashSet<String>,
        rust_const_additions: HashSet<String>,
        in_file_callees: std::collections::HashSet<String>,
        /// new_path の export clause / `pub use` が公開する名前集合 (TS/JS/Rust)。
        /// Phase 0 の単一 parse で先取りし、process_modified_file が再 read+parse
        /// せず使う (perf #2)。
        new_export_surface_names: std::collections::HashSet<String>,
        /// Rust のとき、旧新の公開シンボルの宣言の文脈 (Issue #282)。
        rust_declarations: Option<Box<super::RustDeclarationPair>>,
    },
}

/// Phase 0 の結果。diff ファイルごとの事実と、cross-file 参照判定で検索する名前。
pub(crate) struct PreparedDiff {
    /// `diff_files` と同じ順に並ぶ。
    pub(crate) files: Vec<PreparedDiffFile>,
    /// `ApiRefIndex` で検索する名前。
    pub(crate) index_names: HashSet<String>,
    /// 旧ソースを指定 base と照合できなかった削除ファイルの旧パス。
    pub(crate) unverified_deleted_paths: HashSet<String>,
    /// 読み込み・parse の失敗と、base と照合できなかった旧ソースの申告 (ファイル順)。
    pub(crate) parse_truncations: Vec<crate::models::truncation::TruncationInfo>,
    /// TS の `stripInternal` の設定。公開面の抽出で読んだ設定を、分類でも使う。
    pub(crate) strip_internal: super::ts_strip_internal::StripInternal,
    /// 設定だけの変更で `stripInternal` の当たり方が変わった、差分に無い TS のソース
    /// (Issue #287)。旧版 (base の版の設定) と新版 (今の設定) を変更ファイルと同じく比べる。
    /// `diff_files` には入れない (変えたファイルとして扱わない)。
    pub(crate) rechecks: Vec<(crate::models::impact::DiffFile, PreparedDiffFile)>,
}

/// 設定だけの変更で比べ直す TS のソースの数・1 つの大きさ・合計の上限 (Issue #287)。超えれば
/// 比べ直さず申告する。
const MAX_RECHECK_FILES: usize = 10_000;
const MAX_RECHECK_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_RECHECK_TOTAL_BYTES: usize = 64 * 1024 * 1024;

/// 出力する TS のソースのパスか (宣言ファイルを除く)。
fn is_emitted_ts_path(path: &str) -> bool {
    [".ts", ".tsx", ".mts", ".cts"]
        .iter()
        .any(|suffix| path.ends_with(suffix))
        && !super::ts_module_paths::is_declaration_path(path)
}

/// TS・JS のソースのパスか (宣言ファイルを含む)。
fn is_module_source_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs"]
        .iter()
        .any(|suffix| lower.ends_with(suffix))
}

/// 差分が、設定が入力から取り込むファイルを変え得るか (Issue #288): TS・JS のソースの追加・削除・
/// rename (指定子の解決の候補が変わる) と、取り込み (import・export … from・`require()`・
/// `/// <reference>` など) の変更。読めなければ変え得るとする。
fn changes_module_graph(
    dir: &str,
    df: &crate::models::impact::DiffFile,
    base_blobs: &crate::commands::git_input::GitBlobBatch,
) -> bool {
    if !(is_module_source_path(&df.old_path) || is_module_source_path(&df.new_path)) {
        return false;
    }
    if df.old_path != df.new_path {
        return true;
    }
    if df.hunks.is_empty() {
        return false;
    }
    match (
        base_blobs.read(&df.old_path),
        super::source_pair::load_new_source(dir, &df.new_path),
    ) {
        (Some(old), Some(new)) => {
            super::ts_alias_resolution::imports_differ(&df.new_path, &old, &new)
        }
        _ => true,
    }
}

/// `stripInternal` の判定を決められなかったときの説明。
const STRIP_UNDETERMINED: &str = "a config could not be read or resolved, or whether a config compiles the file through imports could not be determined";

/// `stripInternal` の判定を決められなかったときの申告。
fn strip_internal_unavailable(
    path: Option<&str>,
    message: &str,
) -> crate::models::truncation::TruncationInfo {
    crate::models::truncation::TruncationInfo::new(
        path.map(str::to_owned),
        crate::models::truncation::TruncationReason::TsStripInternalUnavailable,
        format!(
            "TS stripInternal could not be compared between the base and current configs: {message}"
        ),
    )
}

impl PreparedDiff {
    /// added / modified / deleted ファイルの exported シンボルを抽出し、cross-file 参照判定の
    /// 対象になりうる名前を集める。候補シンボルごとの全リポジトリ走査 (O(候補数 × 全ファイル))
    /// を `ApiRefIndex` の chunk 単位の batch 検索に集約するため、検索する名前をここで決める。
    ///
    /// 生成物の判定の属性は `canonical_dir` で解決する。解決できずに対象に残したファイルは
    /// `parse_truncations` で申告する。
    /// `renames` は内容が同じ rename (`diff_files` には入れない)。旧新のパスに当たる
    /// `stripInternal` の設定が違うものだけを比べ直す (Issue #287)。
    pub(crate) fn new(
        dir: &str,
        diff_files: &[crate::models::impact::DiffFile],
        renames: &[crate::models::impact::DiffFile],
        canonical_dir: Option<&std::path::Path>,
        base_blobs: &crate::commands::git_input::GitBlobBatch,
        rust_crates: &super::RustCrates<'_>,
    ) -> Self {
        let mut prepared = Self {
            files: Vec::with_capacity(diff_files.len()),
            index_names: HashSet::new(),
            unverified_deleted_paths: HashSet::new(),
            parse_truncations: Vec::new(),
            strip_internal: super::ts_strip_internal::StripInternal::disabled(),
            rechecks: Vec::new(),
        };
        let resolver = canonical_dir.map(crate::engine::gitattributes::AttributeResolver::open);
        let mut lookup = resolver.as_ref().map(|resolver| resolver.lookup());
        let mut undetermined = Vec::new();
        // 旧版のソースは base の版の設定で、新版は今の設定で判定する (Issue #287)。
        let strip_internal =
            super::ts_strip_internal::StripInternal::with_base(dir, base_blobs.rev());
        for df in diff_files {
            let file = prepared.prepare_file(
                dir,
                df,
                lookup.as_mut(),
                canonical_dir,
                base_blobs,
                &mut undetermined,
                &strip_internal,
            );
            prepared.files.push(file);
        }
        // 内容が同じ rename でも、旧新のパスに当たる `stripInternal` の設定が違えば、宣言
        // ファイルの面が変わる (Issue #287)。生成物は通常の差分と同じく外す。
        for df in renames {
            if df.old_path != df.new_path
                && is_emitted_ts_path(&df.new_path)
                && let Some(source) = super::source_pair::load_new_source(dir, &df.new_path)
                && prepared.strip_changed(&df.old_path, &df.new_path, &source, &strip_internal)
                && !should_skip_diff_file(df, lookup.as_mut(), canonical_dir, &mut undetermined)
            {
                let file = prepared.prepare_modified_file(dir, df, base_blobs, &strip_internal);
                prepared.rechecks.push((df.clone(), file));
            }
        }
        // 設定 (JSON。内容が同じ rename も) を変えた差分 (Issue #287) と、TS・JS のソースの取り込み
        // やファイルの構成を変えた差分 (Issue #288。設定が入力から取り込むファイルが変わる) では、
        // 差分に無い TS のソースも、`stripInternal` の当たり方が変われば比べ直す。
        let configs_changed = diff_files.iter().chain(renames).any(|df| {
            [&df.old_path, &df.new_path]
                .iter()
                .any(|path| path.ends_with(".json"))
        });
        if configs_changed
            || diff_files
                .iter()
                .chain(renames)
                .any(|df| changes_module_graph(dir, df, base_blobs))
        {
            prepared.recheck_strip_internal(
                dir,
                diff_files,
                renames,
                base_blobs,
                &strip_internal,
                lookup.as_mut(),
                canonical_dir,
                &mut undetermined,
            );
        }
        // Rust の lib crate で束縛が変わったときは、それを名指し得る差分に無いソースも比べ直す
        // (Issue #300)。
        prepared.recheck_rust_dependents(
            dir,
            rust_crates,
            lookup.as_mut(),
            canonical_dir,
            &mut undetermined,
            base_blobs,
            &strip_internal,
        );
        if let Some(root) = canonical_dir {
            prepared.parse_truncations.extend(
                crate::engine::refs::generated_undetermined_truncations(root, &undetermined),
            );
        }
        prepared.strip_internal = strip_internal;
        prepared
    }

    /// Rust の lib crate で束縛 (型別名・定数・`use`・glob・`mod` など) が変わった package の、それを
    /// 名指し得る差分に無いソースを、変更ファイルと同じく比べる (Issue #300)。シグネチャは同じなので、
    /// 名指す束縛が変わった宣言だけが blocking な api.mod になる。探し切れなければ申告し、hook を止める。
    #[allow(clippy::too_many_arguments)]
    fn recheck_rust_dependents(
        &mut self,
        dir: &str,
        rust_crates: &super::RustCrates<'_>,
        mut generated: Option<&mut crate::engine::gitattributes::AttributeLookup<'_>>,
        canonical_dir: Option<&std::path::Path>,
        undetermined: &mut Vec<GeneratedUndetermined>,
        base_blobs: &crate::commands::git_input::GitBlobBatch,
        strip_internal: &super::ts_strip_internal::StripInternal,
    ) {
        for (root, pair) in rust_crates.pairs() {
            if let Some(reason) = &pair.incomplete {
                self.parse_truncations
                    .push(crate::models::truncation::TruncationInfo::new(
                        Some(root.clone()),
                        crate::models::truncation::TruncationReason::RustDependentsIncomplete,
                        format!(
                            "Rust sources that may name the changed bindings of this crate were not all compared: {reason}"
                        ),
                    ));
            }
            for (old_path, new_path) in &pair.dependents {
                let df = crate::models::impact::DiffFile {
                    old_path: old_path.clone(),
                    new_path: new_path.clone(),
                    hunks: Vec::new(),
                    deleted_old_source: None,
                };
                // 生成物は通常の差分と同じく外す。
                if should_skip_diff_file(&df, generated.as_deref_mut(), canonical_dir, undetermined)
                {
                    continue;
                }
                let file = self.prepare_modified_file(dir, &df, base_blobs, strip_internal);
                self.rechecks.push((df, file));
            }
        }
    }

    /// 設定や取り込みの変更で `stripInternal` の当たり方が変わる、差分に無い TS のソースを比べ直す
    /// (Issue #287・#288)。追跡している出力する TS のソースのうち `@internal` を含むものについて、
    /// 旧版 (base の版の設定と取り込み) と新版 (今の設定と取り込み) の判定が違えば、変更ファイルと
    /// 同じく比べる。base の版の設定は、`@internal` を含むソースが見つかってから読む (それまで git を
    /// 動かさない)。判定を決められないソースは、まとめて申告する。
    #[allow(clippy::too_many_arguments)]
    fn recheck_strip_internal(
        &mut self,
        dir: &str,
        diff_files: &[crate::models::impact::DiffFile],
        renames: &[crate::models::impact::DiffFile],
        base_blobs: &crate::commands::git_input::GitBlobBatch,
        strip_internal: &super::ts_strip_internal::StripInternal,
        mut generated: Option<&mut crate::engine::gitattributes::AttributeLookup<'_>>,
        canonical_dir: Option<&std::path::Path>,
        undetermined: &mut Vec<GeneratedUndetermined>,
    ) {
        let touched: HashSet<&str> = diff_files
            .iter()
            .chain(renames)
            .flat_map(|df| [df.old_path.as_str(), df.new_path.as_str()])
            .collect();
        let Some(tracked) = tracked_files(dir) else {
            self.parse_truncations.push(strip_internal_unavailable(
                None,
                "the tracked files could not be listed",
            ));
            return;
        };
        let mut files = 0usize;
        let mut bytes = 0usize;
        let mut unread = 0usize;
        let mut base_checked = false;
        let mut undetermined_sources = Vec::new();
        for path in tracked {
            if touched.contains(path.as_str())
                || !is_emitted_ts_path(&path)
                || is_test_path(std::path::Path::new(&path))
                || !crate::engine::impact::is_safe_diff_path(&path)
            {
                continue;
            }
            files += 1;
            if files > MAX_RECHECK_FILES {
                self.parse_truncations.push(strip_internal_unavailable(
                    None,
                    &format!("more than {MAX_RECHECK_FILES} TS sources to recheck"),
                ));
                return;
            }
            let full = std::path::Path::new(dir).join(&path);
            let Some(source) =
                crate::engine::bounded_read::read_file_limited(&full, MAX_RECHECK_FILE_BYTES)
            else {
                unread += 1;
                continue;
            };
            bytes += source.len();
            if bytes > MAX_RECHECK_TOTAL_BYTES {
                self.parse_truncations.push(strip_internal_unavailable(
                    None,
                    &format!("more than {MAX_RECHECK_TOTAL_BYTES} bytes of TS sources to recheck"),
                ));
                return;
            }
            let may_strip = parser::detect_lang(camino::Utf8Path::new(&path), &source)
                .is_ok_and(|lang| super::ts_strip_internal::may_strip(&path, lang, &source));
            if !may_strip {
                continue;
            }
            if !base_checked {
                base_checked = true;
                if !strip_internal.has_base() {
                    self.parse_truncations.push(strip_internal_unavailable(
                        None,
                        "the base revision's configs could not be read",
                    ));
                    return;
                }
            }
            match Self::strip_comparison(&path, &path, &source, strip_internal) {
                Some(true) => {}
                Some(false) => continue,
                None => {
                    undetermined_sources.push(path);
                    continue;
                }
            }
            let df = crate::models::impact::DiffFile {
                old_path: path.clone(),
                new_path: path,
                hunks: Vec::new(),
                deleted_old_source: None,
            };
            // 生成物は通常の差分と同じく外す。
            if should_skip_diff_file(&df, generated.as_deref_mut(), canonical_dir, undetermined) {
                continue;
            }
            let file = self.prepare_modified_file(dir, &df, base_blobs, strip_internal);
            self.rechecks.push((df, file));
        }
        if unread > 0 {
            self.parse_truncations.push(strip_internal_unavailable(
                None,
                &format!(
                    "{unread} TS sources could not be read (or exceed {MAX_RECHECK_FILE_BYTES} bytes)"
                ),
            ));
        }
        if !undetermined_sources.is_empty() {
            const EXAMPLES: usize = 5;
            let mut examples =
                undetermined_sources[..undetermined_sources.len().min(EXAMPLES)].join(", ");
            if undetermined_sources.len() > EXAMPLES {
                examples.push_str(", ...");
            }
            self.parse_truncations.push(strip_internal_unavailable(
                None,
                &format!(
                    "{} TS sources were not rechecked ({STRIP_UNDETERMINED}): {examples}",
                    undetermined_sources.len()
                ),
            ));
        }
    }

    /// ソース `source` (新版 `new_path`、旧版 `old_path`) に当たる `stripInternal` の判定が旧新で
    /// 変わったか。決められなければ申告して false (確定した差分にしない)。
    fn strip_changed(
        &mut self,
        old_path: &str,
        new_path: &str,
        source: &[u8],
        strip_internal: &super::ts_strip_internal::StripInternal,
    ) -> bool {
        match Self::strip_comparison(old_path, new_path, source, strip_internal) {
            Some(changed) => changed,
            None => {
                self.parse_truncations.push(strip_internal_unavailable(
                    Some(new_path),
                    STRIP_UNDETERMINED,
                ));
                false
            }
        }
    }

    /// [`Self::strip_changed`] の判定。決められなければ None。`@internal` を含まない・出力しない
    /// ソースと、base の版の設定を読めないときは変わらないとする。
    fn strip_comparison(
        old_path: &str,
        new_path: &str,
        source: &[u8],
        strip_internal: &super::ts_strip_internal::StripInternal,
    ) -> Option<bool> {
        let Ok(lang) = parser::detect_lang(camino::Utf8Path::new(new_path), source) else {
            return Some(false);
        };
        if !super::ts_strip_internal::may_strip(new_path, lang, source) {
            return Some(false);
        }
        let Some((old, new)) = strip_internal.states(old_path, new_path) else {
            return Some(false);
        };
        use super::ts_alias_resolution::StripState;
        if old == StripState::Unknown || new == StripState::Unknown {
            return None;
        }
        Some(old != new)
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_file(
        &mut self,
        dir: &str,
        df: &crate::models::impact::DiffFile,
        generated: Option<&mut crate::engine::gitattributes::AttributeLookup<'_>>,
        canonical_dir: Option<&std::path::Path>,
        base_blobs: &crate::commands::git_input::GitBlobBatch,
        undetermined: &mut Vec<GeneratedUndetermined>,
        strip_internal: &super::ts_strip_internal::StripInternal,
    ) -> PreparedDiffFile {
        if should_skip_diff_file(df, generated, canonical_dir, undetermined) {
            return PreparedDiffFile::Skip;
        }
        if df.old_path == "/dev/null" {
            return self.prepare_added_file(dir, df, strip_internal);
        }
        if df.new_path == "/dev/null" {
            return self.prepare_deleted_file(df, base_blobs, "deleted file", strip_internal);
        }
        // hunk の無い rename (内容同一) はシンボルが変わらない。旧側が拡張子で判定できる
        // ソースで、新側が対応言語の外へ出た (拡張子でも shebang でも言語を判定できない) 場合
        // だけ、旧ファイルの削除として扱う。内容が同一なので、旧側を拡張子で判定できなければ
        // 旧側の言語も新側と同じ shebang で決まり、API 面は変わらない。
        if df.hunks.is_empty() {
            let leaves_source = renames_out_of_known_extension(df)
                && extract_new_file_facts(dir, &df.new_path, strip_internal).language_unknown;
            if leaves_source {
                return self.prepare_deleted_file(df, base_blobs, "renamed file", strip_internal);
            }
            // 内容が同じ rename でも、旧新のパスに当たる `stripInternal` の設定が違えば、宣言
            // ファイルの面が変わる (Issue #287)。
            if df.old_path != df.new_path
                && is_emitted_ts_path(&df.new_path)
                && let Some(source) = super::source_pair::load_new_source(dir, &df.new_path)
                && self.strip_changed(&df.old_path, &df.new_path, &source, strip_internal)
            {
                return self.prepare_modified_file(dir, df, base_blobs, strip_internal);
            }
            return PreparedDiffFile::Skip;
        }
        self.prepare_modified_file(dir, df, base_blobs, strip_internal)
    }

    /// 新規ファイル (`old_path == "/dev/null"`)。
    fn prepare_added_file(
        &mut self,
        dir: &str,
        df: &crate::models::impact::DiffFile,
        strip_internal: &super::ts_strip_internal::StripInternal,
    ) -> PreparedDiffFile {
        // new_path を 1 回 read+parse して exported / callees を導出 (perf #2)。
        let mut facts = extract_new_file_facts(dir, &df.new_path, strip_internal);
        self.parse_truncations.extend(facts.parse_truncations);
        let rust_declarations = facts
            .exported
            .as_mut()
            .and_then(|exported| exported.rust_declarations.take());
        let new_syms = facts.exported.map(|facts| facts.symbols);
        let in_file_callees = facts.callees;
        if let Some(syms) = &new_syms {
            for (name, _, _) in syms {
                // per-symbol 実装と同じ短絡: ファイル内から呼ばれるシンボルは
                // `is_used_in_diff_paths` に到達しないため検索対象に入れない。
                if !is_internally_connected(&in_file_callees, name) {
                    self.index_names.insert(bare_name(name).to_string());
                }
            }
        }
        PreparedDiffFile::Added {
            new_syms,
            in_file_callees,
            rust_declarations,
        }
    }

    /// 旧ファイルの削除として扱うファイル (削除と、対応言語の外への内容同一の rename)。
    /// `subject` は base と照合できなかったときの申告の主語 (`deleted file` など)。
    fn prepare_deleted_file(
        &mut self,
        df: &crate::models::impact::DiffFile,
        base_blobs: &crate::commands::git_input::GitBlobBatch,
        subject: &str,
        strip_internal: &super::ts_strip_internal::StripInternal,
    ) -> PreparedDiffFile {
        let facts = extract_deleted_file_exported_symbols(base_blobs, df, strip_internal);
        if facts.old_source_unverified && facts.analyzable_source {
            self.unverified_deleted_paths.insert(df.old_path.clone());
            self.parse_truncations
                .push(crate::models::truncation::TruncationInfo::new(
                    Some(df.old_path.clone()),
                    crate::models::truncation::TruncationReason::DiffBaseMismatch,
                    format!("{subject}'s old source cannot be verified against the requested base"),
                ));
        }
        collect_deleted_bash_index_names(df, facts.symbols.as_deref(), &mut self.index_names);
        PreparedDiffFile::Deleted {
            old_syms: facts.symbols,
            old_source_unverified: facts.old_source_unverified,
            rust_declarations: facts.rust_declarations,
        }
    }

    /// 旧版と新版の両方があるファイル。
    fn prepare_modified_file(
        &mut self,
        dir: &str,
        df: &crate::models::impact::DiffFile,
        base_blobs: &crate::commands::git_input::GitBlobBatch,
        strip_internal: &super::ts_strip_internal::StripInternal,
    ) -> PreparedDiffFile {
        // 旧新の片方の `stripInternal` の判定を決められなければ、項目ごとに当て方を決め
        // ([`super::ts_strip_internal::StripInternal::align`])、申告する (Issue #287・#288)。旧版に
        // だけ `@internal` がある (最後のタグを消した) 変更も見る。
        let may_strip = |path: &str, source: &[u8]| {
            parser::detect_lang(camino::Utf8Path::new(path), source)
                .is_ok_and(|lang| super::ts_strip_internal::may_strip(path, lang, source))
        };
        if is_emitted_ts_path(&df.old_path) || is_emitted_ts_path(&df.new_path) {
            let new_source = super::source_pair::load_new_source(dir, &df.new_path);
            let old_source = base_blobs.read(&df.old_path);
            if (new_source
                .as_ref()
                .is_some_and(|source| may_strip(&df.new_path, source))
                || old_source
                    .as_ref()
                    .is_some_and(|source| may_strip(&df.old_path, source)))
                && strip_internal.align(
                    &df.old_path,
                    &df.new_path,
                    old_source.as_deref(),
                    new_source.as_deref(),
                )
            {
                self.parse_truncations.push(strip_internal_unavailable(
                    Some(&df.new_path),
                    &format!(
                        "{STRIP_UNDETERMINED}; only changes in the source that break either way are compared"
                    ),
                ));
            }
        }
        // rename 差分では base 側に新パスが存在しないため、旧版は old_path から読む。
        let mut old_facts =
            extract_exported_file_facts_from_git(base_blobs, &df.old_path, strip_internal);
        // new_path を 1 回 read+parse して exported / callees / export surface を導出 (perf #2)。
        // export surface は process_modified_file が再 parse せず使えるよう
        // PreparedDiffFile に持たせる。
        let mut facts = extract_new_file_facts(dir, &df.new_path, strip_internal);
        self.parse_truncations.extend(facts.parse_truncations);
        // 対応言語でないファイルへの rename (`api.ts` → `api.ts.bak`) は、新側に API 面が
        // 無いので旧ファイルの削除と同じ。変更ファイルとして扱うと新側のシンボルが取れず
        // 何も検出されない (旧 API がすべて消えるのに api.rm が出ない)。
        // 読み込み / parse の失敗は「API 面が無い」証明ではないので対象外。同じパスのまま
        // shebang を消した拡張子なしスクリプトのような in-place 変更も、パスで参照する
        // 利用側は壊れないので対象外 (rename に限る)。
        if df.old_path != df.new_path
            && old_facts.is_some()
            && facts.exported.is_none()
            && facts.language_unknown
        {
            let rust_declarations = old_facts
                .as_mut()
                .and_then(|old| old.rust_declarations.take());
            let old_syms = old_facts.map(|facts| facts.symbols);
            collect_deleted_bash_index_names(df, old_syms.as_deref(), &mut self.index_names);
            return PreparedDiffFile::Deleted {
                old_syms,
                old_source_unverified: false,
                rust_declarations,
            };
        }
        // 同じ parse で得た証拠を比較し、以後は証明済みの名前だけを運ぶ。
        let (parameter_pattern_only_names, rust_const_additions) = if df.old_path == df.new_path {
            old_facts
                .as_ref()
                .zip(facts.exported.as_ref())
                .map(|(old, new)| {
                    (
                        old.parameter_pattern_only_names(new),
                        old.const_addition_names(new, || {
                            load_old_source_with_new(
                                base_blobs,
                                &df.old_path,
                                &df.new_path,
                                load_new_source(dir, &df.new_path)?,
                            )
                        }),
                    )
                })
                .unwrap_or_default()
        } else {
            (HashSet::new(), HashSet::new())
        };
        let type_dependencies = old_facts
            .as_mut()
            .and_then(|old| old.type_dependencies.take())
            .zip(
                facts
                    .exported
                    .as_mut()
                    .and_then(|new| new.type_dependencies.take()),
            )
            .map(|(old, new)| old.compare(new))
            .unwrap_or_default();
        let rust_declarations = old_facts
            .as_mut()
            .and_then(|old| old.rust_declarations.take())
            .zip(facts.exported.as_mut().and_then(|new| {
                let declarations = new.rust_declarations.take()?;
                Some((declarations, std::mem::take(&mut new.rust_shared_callables)))
            }))
            .map(|(old, (new, new_shared_callables))| {
                Box::new(super::RustDeclarationPair {
                    old,
                    new,
                    new_shared_callables,
                })
            });
        let old_syms = old_facts.map(|facts| facts.symbols);
        let new_syms = facts.exported.map(|facts| facts.symbols);
        let in_file_callees = facts.callees;
        let new_export_surface_names = facts.export_surface_names;
        if let (Some(old), Some(new)) = (&old_syms, &new_syms) {
            self.index_names.extend(
                new.iter()
                    .filter(|(name, _, _)| {
                        type_dependencies.get(name)
                            == Some(&ts_signature::DependencyStatus::Changed)
                    })
                    .map(|(name, _, _)| bare_name(name).to_owned()),
            );
            collect_modified_file_index_names(
                old,
                new,
                &in_file_callees,
                &parameter_pattern_only_names,
                is_bash_script_path(&df.old_path),
                ts_namespace::recovery_language(&df.old_path, &df.new_path),
                &mut self.index_names,
            );
        }
        PreparedDiffFile::Modified {
            type_dependencies: Box::new(type_dependencies),
            unparsed_declarations: facts.unparsed_declarations,
            old_syms,
            new_syms,
            parameter_pattern_only_names,
            rust_const_additions,
            in_file_callees,
            new_export_surface_names,
            rust_declarations,
        }
    }
}

/// 削除扱いにしたファイル (削除 / 対応言語外への rename) が bash なら、関数名を
/// ApiRefIndex の検索対象に入れる。残存参照判定 (`is_removed_bash_symbol_unreferenced`) を
/// 関数ごとの全リポジトリ走査ではなく index から引くため。bash の関数名は `.` を含みうるので
/// bare name に畳まず、そのままの名前で検索する。
fn collect_deleted_bash_index_names(
    df: &crate::models::impact::DiffFile,
    old_syms: Option<&[(String, String, String)]>,
    index_names: &mut HashSet<String>,
) {
    if is_bash_script_path(&df.old_path)
        && let Some(syms) = old_syms
    {
        index_names.extend(syms.iter().map(|(name, _, _)| name.clone()));
    }
}

/// modified ファイルで cross-file 参照判定の対象になりうる name を `index_names` へ集める。
/// - 新規追加 (new のみ): `is_used_in_diff_paths` が bare name で検索
/// - 削除 (old のみ): `is_removed_symbol_unreferenced` が bare name で検索
/// - シグネチャ変更: `has_cross_file_refs` が exact name、`is_modified_closed_in_diff` /
///   `has_blocking_value_usage` が bare name で検索
///
/// per-symbol 実装の短絡で検索に到達しえない name は収集しない (過剰収集は AC trie の
/// パターンと事前フィルタのヒット (= parse 対象ファイル) を増やし、小規模 diff で
/// per-symbol より遅くなる):
/// - 新規追加: `is_internally_connected` が true なら `is_used_in_diff_paths` 未到達
/// - シグネチャ変更: exact name (`has_cross_file_refs`) は
///   `is_internally_connected && !has_cross_file_refs` の短絡により internally connected
///   のときのみ評価される
/// - 削除: `(!new_symbols_in_current_file.is_empty() || bash_pure_removal_skip)` が
///   成立しなければ `is_removed_symbol_unreferenced` 未到達 (bash の git show 判定は
///   再現せず `is_bash_old_file` で過剰側に倒す)
///
/// bin-only crate / private module / 同名複数等の残りの篩いは再現せず過剰側に倒す。
/// 過剰収集は検索コストが増えるだけで判定結果には影響しない (逆に収集漏れは
/// `refs_for` が `None` を返して保守側に倒れ、判定が変わりうる)。
pub(crate) fn collect_modified_file_index_names(
    old_syms: &[(String, String, String)],
    new_syms: &[(String, String, String)],
    in_file_callees: &std::collections::HashSet<String>,
    parameter_pattern_only_names: &HashSet<String>,
    is_bash_old_file: bool,
    lang: Option<crate::language::LangId>,
    index_names: &mut HashSet<String>,
) {
    if matches!(
        lang,
        Some(crate::language::LangId::Typescript | crate::language::LangId::Tsx)
    ) {
        index_names.extend(super::ts_namespace::changed_duplicate_names(
            old_syms, new_syms,
        ));
    }
    let old_map: std::collections::HashMap<&str, &str> = old_syms
        .iter()
        .map(|(name, _kind, sig)| (name.as_str(), sig.as_str()))
        .collect();
    let new_names: HashSet<&str> = new_syms.iter().map(|(name, _, _)| name.as_str()).collect();
    let mut has_new_only_symbol = false;
    for (name, _kind, sig) in new_syms {
        match old_map.get(name.as_str()) {
            None => {
                has_new_only_symbol = true;
                if !is_internally_connected(in_file_callees, name) {
                    index_names.insert(bare_name(name).to_string());
                }
            }
            Some(old_sig)
                if old_sig != &sig.as_str() && !parameter_pattern_only_names.contains(name) =>
            {
                // has_cross_file_refs も bare 名で照合するため exact 名 (qualname) の
                // 収集は不要 (identifier ノードに一致せず常に 0 件で AC trie を太らせるだけ)。
                index_names.insert(bare_name(name).to_string());
            }
            Some(_) => {}
        }
    }
    if !(has_new_only_symbol || is_bash_old_file) {
        return;
    }
    for (name, _, _) in old_syms {
        if !new_names.contains(name.as_str()) {
            index_names.insert(bare_name(name).to_string());
        }
    }
}

/// 削除前の公開宣言と、指定 base で旧ソースを検証できたかを一緒に返す。
pub(crate) struct DeletedSourceFacts {
    pub(crate) symbols: Option<ExportedSymbols>,
    /// Rust のとき、`symbols` と同じ順の宣言の文脈 (Issue #282)。
    pub(crate) rust_declarations: Option<Vec<super::RustDeclarationContext>>,
    pub(crate) old_source_unverified: bool,
    pub(crate) analyzable_source: bool,
}

fn is_gitlink_patch_source(source: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(source) else {
        return false;
    };
    let Some(hash) = text
        .trim_end_matches(['\r', '\n'])
        .strip_prefix("Subproject commit ")
    else {
        return false;
    };
    matches!(hash.len(), 40 | 64) && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// base と明示 diff の旧ソースを照合し、採用した旧ソースから公開宣言を抽出する。
pub(crate) fn extract_deleted_file_exported_symbols(
    base_blobs: &crate::commands::git_input::GitBlobBatch,
    df: &crate::models::impact::DiffFile,
    strip_internal: &super::ts_strip_internal::StripInternal,
) -> DeletedSourceFacts {
    let base_source = base_blobs.read(&df.old_path);
    let diff_source = df.deleted_old_source.as_deref();
    // unified diff の復元は CRLF と末尾改行の有無を保持しない。行内容で比較する。
    // hunk が途中で切れた場合は部分ソースを完全な base blob より優先しない。
    let diff_complete = df.hunks.is_empty()
        || (df.hunks.len() == 1
            && df.hunks[0].old_start == 1
            && df.hunks[0].new_start == 0
            && df.hunks[0].new_count == 0
            && diff_source.is_some_and(|src| {
                std::str::from_utf8(src)
                    .is_ok_and(|text| text.lines().count() == df.hunks[0].old_count)
            }));
    let old_source_unverified = match (base_source.as_deref(), diff_source) {
        (Some(base), Some(diff)) if diff_complete => {
            match (std::str::from_utf8(base), std::str::from_utf8(diff)) {
                (Ok(base), Ok(diff)) => !base.lines().eq(diff.lines()),
                _ => base != diff,
            }
        }
        (Some(_), None) => false,
        (Some(_), Some(_)) => true,
        (None, _) => true,
    };
    // base が別時点の blob を指すなら、明示 diff に含まれる旧ソースを優先する。
    let source = if !diff_complete {
        base_source.as_deref().or(diff_source)
    } else if old_source_unverified {
        diff_source.or(base_source.as_deref())
    } else {
        base_source.as_deref()
    };
    let path = camino::Utf8Path::new(&df.old_path);
    let gitlink = base_source.is_none() && diff_source.is_some_and(is_gitlink_patch_source);
    let analyzable_source = !gitlink
        && (crate::language::LangId::from_path(path).is_ok()
            || source.is_some_and(|src| crate::engine::parser::detect_lang(path, src).is_ok()));
    let (symbols, rust_declarations) = source
        .and_then(|src| {
            super::exported::extract_deleted_exported_facts(&df.old_path, src, strip_internal)
        })
        .map_or((None, None), |(symbols, declarations)| {
            (Some(symbols), declarations)
        });
    DeletedSourceFacts {
        symbols,
        rust_declarations,
        old_source_unverified,
        analyzable_source,
    }
}

/// src 相対パスを Rust モジュールセグメント列に変換する。
/// `meeting/macos.rs` → `[meeting, macos]`、`meeting/mod.rs` → `[meeting]`、
/// `lib.rs` / `main.rs` → `[]` (root モジュール)。
pub(crate) fn module_path_segments(rel: &std::path::Path) -> Vec<String> {
    let comps: Vec<_> = rel.components().collect();
    let mut segs: Vec<String> = Vec::new();
    let last = comps.len().saturating_sub(1);
    for (i, c) in comps.iter().enumerate() {
        let name = c.as_os_str().to_string_lossy();
        if i == last {
            let stem = std::path::Path::new(name.as_ref())
                .file_stem()
                .map(|s| s.to_string_lossy().to_string());
            match stem.as_deref() {
                // mod.rs / lib.rs / main.rs はそのディレクトリのモジュール自身を表す
                Some("mod") | Some("lib") | Some("main") => {}
                Some(s) => segs.push(s.to_string()),
                None => {}
            }
        } else {
            segs.push(name.to_string());
        }
    }
    segs
}

/// 親モジュールファイル直下の `mod <mod_name>` 宣言の可視性 (制限なし pub か) を返す。
///
/// source_file 直下の `mod_item` のみを見る。inline mod (`mod foo { mod bar; }`) 内の同名
/// 宣言は別モジュールスコープの宣言なので拾わない (codex 指摘: 再帰探索で別スコープの同名
/// mod を誤って拾うと可視性判定が壊れる)。
pub(crate) fn find_mod_decl_visibility(
    root: tree_sitter::Node<'_>,
    source: &[u8],
    mod_name: &str,
) -> Option<bool> {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "mod_item"
            && child
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
                == Some(mod_name)
        {
            // #[path = "..."] でファイル名と module 名がずれる場合、モジュール解決を諦めて
            // 「判定不能」(None) を返す。下流 (rust_private_module_info_at_base /
            // public_reachable_modules_at_base) は api.rm 抑制を諦め、削除を残す方向に倒す。
            if rust_mod_item_has_path_attribute(child, source) {
                return None;
            }
            let mut mc = child.walk();
            let is_pub = child.children(&mut mc).any(|c| {
                c.kind() == "visibility_modifier" && c.utf8_text(source).map(str::trim) == Ok("pub")
            });
            return Some(is_pub);
        }
    }
    None
}

/// new_path 1 ファイルから API 差分検出に必要な 3 種の facts をまとめて抽出する。
/// 旧 exported / in_file_callees / export surface の各抽出が同一 new_path をそれぞれ
/// read+parse していた (TS/JS/Rust で 3 回、他言語で 2 回) のを **1 回の read+parse に
/// 集約**する (perf #2)。
/// 各抽出のガードは元関数と完全に一致させ behavior-preserving とする:
/// - `exported`: test path は `Some(空)` (parse せず) / 非 test は `is_safe_diff_path` 必須で
///   None と空を区別 / lexer-only は lexer 経由 / それ以外 tree-sitter parse+filter
/// - `callees`: test/safe ガードなし、read+parse 失敗時は空 (lexer-only は parse_source が Err→空)
/// - `export_surface_names`: `is_safe_diff_path` かつ TS/TSX/JS/Rust のみ、それ以外は空。
///   TS/JS は named export clause (from 句の有無を問わず)、Rust は `pub use` が公開する名前
pub(crate) struct NewFileFacts {
    pub(crate) unparsed_declarations:
        Vec<crate::engine::bash_parse_recovery::UnparsedBashDeclaration>,
    pub(crate) parse_truncations: Vec<crate::models::truncation::TruncationInfo>,
    pub(crate) exported: Option<ExportedFileFacts>,
    pub(crate) callees: std::collections::HashSet<String>,
    pub(crate) export_surface_names: std::collections::HashSet<String>,
    /// ファイルは読めたが、拡張子 / shebang から言語を判定できなかった
    /// (= 解析対象のソースではない)。`exported` が `None` になる理由のうち、
    /// 読み込み失敗や parse 失敗 (判定不能) と区別するために持つ。
    pub(crate) language_unknown: bool,
}

pub(crate) fn extract_new_file_facts(
    dir: &str,
    file_path: &str,
    strip_internal: &super::ts_strip_internal::StripInternal,
) -> NewFileFacts {
    let mut facts = NewFileFacts {
        unparsed_declarations: Vec::new(),
        parse_truncations: Vec::new(),
        exported: None,
        callees: std::collections::HashSet::new(),
        export_surface_names: std::collections::HashSet::new(),
        language_unknown: false,
    };
    // exported の test path 短絡: parse せず Some(空) を返す (extract_exported_symbols_from_file と一致)。
    let is_test = is_test_path(std::path::Path::new(file_path));
    if is_test {
        facts.exported = Some(ExportedFileFacts::default());
    }
    // exported (非 test) / reexports は is_safe_diff_path を要求する。callees は要求しない。
    let safe = crate::engine::impact::is_safe_diff_path(file_path);

    let full_path = std::path::Path::new(dir).join(file_path);
    let Some(utf8) = full_path.to_str() else {
        return facts;
    };
    let utf8_path = camino::Utf8Path::new(utf8);
    let Ok(source) = parser::read_file(utf8_path) else {
        return facts;
    };
    let Ok(lang_id) = parser::detect_lang(utf8_path, &source) else {
        facts.language_unknown = true;
        return facts;
    };
    // 片側の `stripInternal` の判定を決められなかったファイルは、残すと決めた項目の `@internal` を
    // 読まない (Issue #288。位置は変わらない)。
    let view = strip_internal.view(false, file_path, lang_id, &source);
    let source: &[u8] = &view;

    // lexer-only (Xojo): tree-sitter parse は呼ばない (parse_source は Err を返すため callees/
    // reexports は元実装でも空)。exported のみ lexer 経由 (非 test かつ safe のとき)。
    if let crate::language::DetectedLang::LexerOnly(lexer_lang) = lang_id.detected() {
        if !is_test && safe {
            facts.exported = Some(ExportedFileFacts::from_symbols(
                crate::engine::lexer::extract_exported_symbols(source, lexer_lang, false),
            ));
        }
        return facts;
    }

    let Ok(tree) = parser::parse_source(source, lang_id) else {
        return facts;
    };
    let root = tree.root_node();

    if lang_id == crate::language::LangId::Bash && root.has_error() {
        use crate::engine::bash_parse_recovery::{parse_error_message, unparsed_declarations};
        facts.unparsed_declarations = unparsed_declarations(root, source);
        facts
            .parse_truncations
            .push(crate::models::truncation::TruncationInfo::new(
                Some(file_path.to_owned()),
                crate::models::truncation::TruncationReason::ParseErrorRegion,
                parse_error_message(file_path, source).to_owned(),
            ));
    }

    // callees: test/safe ガードなし (extract_in_file_callees と一致)。
    facts.callees =
        crate::engine::calls::extract_all_callees(root, source, lang_id).unwrap_or_default();

    // exported (非 test かつ safe): extract_symbols → filter_exported_symbols。
    // extract_symbols 失敗は None のまま (元 _inner の `?` と一致)。
    if !is_test
        && safe
        && let Ok(syms) = crate::engine::symbols::extract_symbols(root, source, lang_id)
    {
        facts.exported = Some(filter_exported_file_facts(
            &syms,
            root,
            source,
            lang_id,
            true,
            false,
            Some(file_path),
            strip_internal.applies(file_path, lang_id, source),
        ));
    }

    // export surface: safe かつ TS/TSX/JS/Rust のみ。
    if safe
        && matches!(
            lang_id,
            crate::language::LangId::Typescript
                | crate::language::LangId::Tsx
                | crate::language::LangId::Javascript
                | crate::language::LangId::Rust
        )
    {
        facts.export_surface_names = match lang_id {
            crate::language::LangId::Rust => {
                crate::engine::symbols::collect_rust_reexported_names(root, source)
            }
            _ => crate::engine::symbols::collect_js_ts_named_export_surface_names(root, source),
        };
        // `stripInternal` で宣言ファイルから消える export 文だけが公開する名前は、公開の面に無い。
        if strip_internal.applies(file_path, lang_id, source) {
            for name in super::ts_strip_internal::internal_only_export_names(root, source) {
                facts.export_surface_names.remove(&name);
            }
        }
    }

    facts
}

/// `dir` で追跡しているファイル (`dir` 相対)。列挙できなければ None。
pub(super) fn tracked_files(dir: &str) -> Option<Vec<String>> {
    let output = crate::git_support::git_command_for_dir(dir)
        .args(["ls-files", "-z"])
        .current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8(path.to_vec()).ok())
        .collect()
}
