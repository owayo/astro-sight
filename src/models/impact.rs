use serde::{Deserialize, Serialize};

use super::skip::SkipInfo;
use super::truncation::TruncationInfo;

/// unified diff から解析した hunk。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HunkInfo {
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
}

/// 変更の影響を受けるシンボル。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AffectedSymbol {
    pub name: String,
    pub kind: String,
    pub change_type: String,
}

/// 検出されたシグネチャ変更。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureChange {
    pub name: String,
    pub old_signature: String,
    pub new_signature: String,
}

/// 変更の影響を受ける呼び出し元。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpactedCaller {
    pub path: String,
    pub name: String,
    pub line: usize,
    /// 影響を引き起こしたシンボル名のリスト
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub symbols: Vec<String>,
    /// receiver-aware 確信度。`exact` / `inferred` / `bare` のいずれか。互換のため省略可。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub confidence: Option<String>,
}

/// 変更内容と hunk 情報を含む解析済み diff ファイル。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffFile {
    pub old_path: String,
    pub new_path: String,
    pub hunks: Vec<HunkInfo>,
    /// 削除ファイル (`new_path == "/dev/null"`) で、unified diff の `-` 行から
    /// 復元した旧側ソース。`extract_exported_symbols_from_git` が base mismatch
    /// 等で失敗した場合のフォールバック AST 解析に使う。新規/変更ファイルでは None。
    #[serde(skip)]
    pub deleted_old_source: Option<Vec<u8>>,
}

/// 単一の変更ファイルに対する影響分析。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileImpact {
    pub path: String,
    pub hunks: Vec<HunkInfo>,
    pub affected_symbols: Vec<AffectedSymbol>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub signature_changes: Vec<SignatureChange>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub impacted_callers: Vec<ImpactedCaller>,
    /// 確信度の低い caller (BareNameOnly + generic method name 等)。
    /// `impacted_callers` のシグナルを破壊しないよう別フィールドで保持する。
    /// 空の場合は出力に含めない (互換維持)。
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub low_confidence_callers: Vec<ImpactedCaller>,
    /// import/re-export 行など、存在・名前・シグネチャが維持される限り実装対応不要な参照。
    /// `impacted_callers` の blocking 信号から分離し、レビュー時の優先度を明確にする。
    /// 空の場合は出力に含めない (互換維持)。
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub informational_callers: Vec<ImpactedCaller>,
}

/// context（影響分析）のレスポンスエンベロープ。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContextResult {
    pub changes: Vec<FileImpact>,
    /// git 管理外 dir で `--git` が要求され diff を取得できず skip した場合の理由。
    /// 通常の解析結果では `None` (出力に含まれない・後方互換)。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub skipped: Option<SkipInfo>,
    /// 解析対象から意図的に外したもの (未追跡の巨大ファイル等)。`skipped` が
    /// 「コマンド全体を解析しなかった」大域 skip なのに対し、これは部分的な打ち切り。
    /// 空なら出力に含まれない (compact 規約)。
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub truncations: Vec<TruncationInfo>,
}

/// `analyze_context` / `analyze_impact_streaming` のオプション。
///
/// 既存の固定除外 (`IMPACT_DEFAULT_EXCLUDED_DIRS`: vendor / node_modules /
/// target / build 等) に **追加** で除外したいリポジトリ固有名 (例:
/// `pjproject-2.15`, `openssl_64_1.1.1c`, `third_party`) や glob パターンを
/// ユーザー指定で受け付ける。
///
/// `ASTRO_SIGHT_INCLUDE_VENDOR_FOR_IMPACT=1` と併用した場合、デフォルト除外
/// リストだけが解除され、ユーザー指定の `exclude_dirs` / `exclude_globs` は
/// 引き続き適用される。
#[derive(Debug, Clone, Default)]
pub struct ContextAnalysisOptions {
    /// パスセグメント完全一致で除外するディレクトリ名。
    pub exclude_dirs: Vec<String>,

    /// workspace-relative の glob パターン。`refs::collect_files_with_excludes`
    /// で negative override として扱われる (先頭の `!` は不要)。
    pub exclude_globs: Vec<String>,
}
