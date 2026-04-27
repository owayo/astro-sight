use serde::Serialize;

use super::impact::ContextResult;

/// review コマンドの統合レスポンス。
///
/// `test_only_symbols` は production 側コードから参照されず、
/// test/spec 配下からのみ参照される公開シンボル。dead 同等扱いにすると
/// 「テスト経由で実利用されている API」を誤って除去候補に出してしまうため、
/// 別バケットに分離してレビュアー判断に委ねる。
#[derive(Debug, Clone, Serialize)]
pub struct ReviewResult {
    pub impact: ContextResult,
    pub missing_cochanges: Vec<MissingCochange>,
    pub api_changes: ApiChanges,
    pub dead_symbols: Vec<DeadSymbol>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub test_only_symbols: Vec<DeadSymbol>,
}

/// cochange で検出された「一緒に変更されるはずだが diff に含まれないファイル」。
#[derive(Debug, Clone, Serialize)]
pub struct MissingCochange {
    pub file: String,
    pub expected_with: String,
    pub confidence: f64,
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
#[derive(Debug, Clone, Serialize)]
pub struct ApiChanges {
    pub added: Vec<ApiSymbol>,
    pub removed: Vec<ApiSymbol>,
    pub modified: Vec<ApiSymbolChange>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub moved: Vec<MovedSymbol>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub property_to_field: Vec<PropertyToFieldChange>,
}

/// 公開シンボル情報。
#[derive(Debug, Clone, Serialize)]
pub struct ApiSymbol {
    pub name: String,
    pub kind: String,
    pub file: String,
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
}

/// 参照カウント 0 の公開シンボル。
#[derive(Debug, Clone, Serialize)]
pub struct DeadSymbol {
    pub name: String,
    pub kind: String,
    pub file: String,
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
