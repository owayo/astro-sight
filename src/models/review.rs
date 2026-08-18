use serde::Serialize;

use super::cochange::CoChangeDiagnostics;
use super::impact::ContextResult;
use super::skip::SkipInfo;

/// review コマンドの統合レスポンス。
///
/// `test_only_symbols` は production 側コードから参照されず、
/// test/spec 配下からのみ参照される公開シンボル。dead 同等扱いにすると
/// 「テスト経由で実利用されている API」を誤って除去候補に出してしまうため、
/// 別バケットに分離してレビュアー判断に委ねる。
#[derive(Debug, Clone, Default, Serialize)]
pub struct ReviewResult {
    pub impact: ContextResult,
    pub missing_cochanges: Vec<MissingCochange>,
    /// cochange 解析の内訳。`missing_cochanges` が空のとき「変更漏れが無い」のか
    /// 「起点の証拠を作れなかった / 閾値で落ちた」のかを区別するために付ける。
    /// 解析自体を行わなかった場合は省略される。
    #[serde(skip_serializing_if = "CoChangeDiagnostics::is_empty", default)]
    pub cochange_diagnostics: CoChangeDiagnostics,
    pub api_changes: ApiChanges,
    pub dead_symbols: Vec<DeadSymbol>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub test_only_symbols: Vec<DeadSymbol>,
    /// git 管理外 dir で `--git` が要求され diff を取得できず skip した場合の理由。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub skipped: Option<SkipInfo>,
    /// 解析対象から意図的に外したもの (未追跡の巨大ファイル等)。
    /// `impact.truncations` には入れず review 直下に集約する (二重報告を避ける)。
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub truncations: Vec<crate::models::truncation::TruncationInfo>,
}

/// cochange で検出された「一緒に変更されるはずだが diff に含まれないファイル」。
///
/// `confidence` は raw ratio (`co_changes / denominator`) なので、1/1 と 5/5 が
/// どちらも 1.0 になる。標本の大きさが出力から読めないと「100% 共変更」という
/// 表示だけが独り歩きしてトリアージが空振りするため、分子・分母も併記する
/// (review 経路では `REVIEW_COCHANGE_MIN_SAMPLES` で分子の下限を課しているが、
/// それでも 3/3 と 20/20 では判断が変わる)。
#[derive(Debug, Clone, Serialize)]
pub struct MissingCochange {
    pub file: String,
    pub expected_with: String,
    pub confidence: f64,
    /// 両方のファイルが変更されたコミット数 (`confidence` の分子)。
    pub co_changes: usize,
    /// 実際に集計対象となったコミット数 (`confidence` の分母)。
    /// 算出できなかった場合は省略する。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub denominator: Option<usize>,
    /// この証拠がどう作られたか。既定の `Blame` (変更行 blame) では省略し、
    /// ファイル履歴 fallback (`History`) のときだけ出す。
    ///
    /// 変更行 blame は「その変更が触った行を誰が最後に書いたか」なので相関が今回の変更に
    /// 紐づくが、history fallback は「ファイル全体の直近コミット」なので今回の変更内容とは
    /// 無関係な共変更まで含む。同じ `c` / `n` / `d` でも証拠の強さが違うため区別できるようにする。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub evidence: Option<crate::models::cochange::CoChangeEvidence>,
}

/// 公開シンボルの変更サマリ。
///
/// `moved` は同一コミット内で「ある file から消えたシンボル」と「別 file に追加された
/// 同名・同種別・同シグネチャのシンボル」が一致した場合に 1 件にまとめる。module →
/// package 化リファクタや git rename 未検出時の add/rm ペアを informational として
/// 提示し、`removed`/`added` の誤検出ノイズを抑える。
///
/// `property_to_field` は Python の `@property def x(self) -> T` を `@dataclass` の
/// インスタンスフィールド `x: T` に置き換えたケース。`obj.x` 属性アクセスとしての
/// 公開面は維持されているため、破壊的削除ではなく informational として提示する。
///
/// `removed_dead` は「削除後 HEAD ツリーで他ファイル参照 0 件」の dead-code 整理。
/// `removed` (破壊的 API 削除) と区別して informational として提示することで、
/// レビュー側で「破壊的削除」と「dead-code 掃除」を即座に区別できる。
/// repo 内到達性 0 を保証するが、外部パッケージ利用者ゼロまでは保証しない
/// (Issue 2026-05-28-meet-virtual-you-gemini-multi-select 対応)。
#[derive(Debug, Clone, Default, Serialize)]
pub struct ApiChanges {
    pub added: Vec<ApiSymbol>,
    pub removed: Vec<ApiSymbol>,
    pub modified: Vec<ApiSymbolChange>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub moved: Vec<MovedSymbol>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub property_to_field: Vec<PropertyToFieldChange>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub removed_dead: Vec<ApiSymbol>,
    /// シグネチャ変更だが、全 cross-file 参照が同一 diff 内の変更 hunk で追随済みの api.mod。
    /// 呼び出し側が同一コミットで更新済みのため破壊的でなく、stop hook をブロックしない
    /// informational 扱い (Issue 2026-05-29-swift-sidecar-api-mod パターンA)。
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub modified_closed_in_diff: Vec<ApiSymbolChange>,
    /// `pub const` / 非 mut `pub static` / `export const` の shape (名前・型・visibility・
    /// binding kind) は不変で initializer (値) のみ変更されたケース。値変更はコンパイル
    /// 互換性を壊さないため `modified` (api.mod) とは別カテゴリとして informational に扱い、
    /// デフォルトでは stop hook をブロックしない。`--strict-public-const-values` 指定時のみ
    /// blocking に昇格する (Issue 2026-06-02-balance-const-value-changes 対応)。
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub const_value_changes: Vec<ApiSymbolChange>,
    /// シグネチャ文字列は変わったが公開契約 (呼び出し側の互換性) が維持される互換 api.mod。
    /// React component の HOC ラップ (`memo` / `forwardRef`) や、未参照プロパティのみ削除した
    /// exported object 等。非 blocking の informational 扱い
    /// (Issue 2026-06-02-react-memo / 2026-06-02-provider-avatar 対応)。
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub compatible_modified: Vec<CompatibleApiModification>,
}

/// 公開シンボル情報。
#[derive(Debug, Clone, Serialize)]
pub struct ApiSymbol {
    pub name: String,
    pub kind: String,
    pub file: String,
    /// 同一ファイル内の実利用参照数 (定義行 / import・re-export 行を除く)。
    ///
    /// `api.add` の抽出条件は「同一ファイル内の**呼び出し**参照が無い」+「同一 diff 内の
    /// 他ファイルからの実利用参照が無い」の合成で、TS の型注釈のように呼び出しではない
    /// 参照は条件に現れない。そのため出力だけでは「同一ファイル内に参照があるのか」
    /// 「完全に未参照なのか」を区別できず、トリアージ側が `refs` を再実行していた
    /// (Issue 2026-08-04-review-add-scope-naming)。
    ///
    /// `added` 経路でのみ算出する。他バケット (`removed` / `removed_dead`) は 0 のままで、
    /// 0 は JSON 出力から省略される (compact 規約)。
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub refs_internal: usize,
}

/// `serde(skip_serializing_if)` 用: 参照数 0 は出力から省略する (compact 規約)。
pub(crate) fn is_zero_usize(value: &usize) -> bool {
    *value == 0
}

/// `serde(skip_serializing_if)` 用: false のフラグは出力から省略する (compact 規約)。
pub(crate) fn is_false(value: &bool) -> bool {
    !*value
}

/// シグネチャが変更された公開シンボル。
#[derive(Debug, Clone, Serialize)]
pub struct ApiSymbolChange {
    pub name: String,
    pub kind: String,
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_signature: Option<String>,
    /// リポジトリ内に解決できた呼び出し参照が 1 件も無い (定義・import/use 行を除く)。
    ///
    /// `api.mod` は「未更新の呼び出し側があるかもしれない」という警告だが、呼び出し参照が
    /// 0 件のシンボルでは更新すべき呼び出し側が存在せず、出力だけではその区別がつかない
    /// (Issue 2026-08-05-api-mod-callers-updated-indirectly のパターン B)。
    ///
    /// **カテゴリ分離も blocking 解除もしない**: 参照 0 件は「未使用」ではなく
    /// 「astro-sight が解決できる内部参照が 0 件」でしかなく、外部リポジトリからの利用・
    /// 動的呼び出し・フレームワーク経由・文字列参照と区別できない。公開ライブラリでは
    /// むしろ最重要の破壊的変更になる。トリアージが「呼び出し側を探す」段階を飛ばせる
    /// ようフラグだけ添える (`ApiSymbol::refs_internal` と同じ方針)。
    ///
    /// `modified` 経路でのみ算出する。他バケットは false のままで JSON からは省略される。
    #[serde(default, skip_serializing_if = "is_false")]
    pub no_resolved_internal_callers: bool,
}

/// 互換性ありと判定された api.mod。シグネチャ文字列は変わったが公開契約 (呼び出し側の
/// 互換性) は維持されるため、非 blocking の informational 扱いとする。`reason` で互換と
/// 判定した根拠を示す。
#[derive(Debug, Clone, Serialize)]
pub struct CompatibleApiModification {
    pub name: String,
    pub kind: String,
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_signature: Option<String>,
    /// 互換と判定した根拠 ("react_component_wrapper" / "unused_object_members")。
    pub reason: String,
}

/// 参照カウント 0 の公開シンボル。
#[derive(Debug, Clone, Serialize)]
pub struct DeadSymbol {
    pub name: String,
    pub kind: String,
    pub file: String,
    /// 宣言行 (0-indexed)。`file` だけでは利用者が結局シンボルを探し直す必要があり、
    /// 「識別子検索を AST に置き換える」という本ツールの目的と噛み合わないため付ける。
    /// 宣言行を解決できなかった場合のみ省略される。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub line: Option<usize>,
}

/// 別ファイルへ移動された公開シンボル。
///
/// 同一コミット内で `from` ファイルから消えたシンボルと、`to` ファイルに追加された
/// 同名・同種別・同シグネチャのシンボルが対応するときに生成される。
#[derive(Debug, Clone, Serialize)]
pub struct MovedSymbol {
    pub name: String,
    pub kind: String,
    pub from: String,
    pub to: String,
}

/// Python の `@property` メソッドを dataclass フィールドへ置き換えた変更。
///
/// `Container.member` という qualname 形式で表現され、旧 tree でメソッド定義として
/// 検出されていたシンボルが、新 tree の同名クラス内で `member: type` のフィールド宣言
/// として残っているケースを表す。`obj.member` 属性アクセスとしての公開面は維持される
/// ため、破壊的削除ではなく informational として提示する。
#[derive(Debug, Clone, Serialize)]
pub struct PropertyToFieldChange {
    pub name: String,
    pub file: String,
}
