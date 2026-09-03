use serde::{Deserialize, Serialize};

use super::result_summary::{ResultSummary, RollupRecord};
use super::skip::SkippedFiles;

/// 参照の種類（定義または利用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefKind {
    #[serde(rename = "def")]
    Definition,
    #[serde(rename = "ref")]
    Reference,
}

/// 参照の確信度レベル。
///
/// - `ExactOwner`: receiver の型が明示されている (`Foo::bar()`, `[Foo::class, "bar"]` 等)。
///   高確信度で当該 owner の method 呼び出しと判定できる。
/// - `InferredOwner`: PHPDoc `@var Foo $x` や `Foo $x` パラメータ型注釈、`new Foo()` の
///   戻り値型など、AST から型を推論可能なケース。中確信度。
/// - `BareNameOnly`: `->bar()` のように receiver の型が分からない呼び出し。
///   `new`/`update`/`save` などの汎用名では impact から除外して `low_confidence_callers`
///   に分離する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefConfidence {
    #[serde(rename = "exact")]
    ExactOwner,
    #[serde(rename = "inferred")]
    InferredOwner,
    #[serde(rename = "bare")]
    BareNameOnly,
}

/// ファイル内にあるシンボルへの単一参照。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolReference {
    pub path: String,
    #[serde(rename = "ln")]
    pub line: usize,
    #[serde(rename = "col")]
    pub column: usize,
    #[serde(rename = "ctx", skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<RefKind>,
    /// receiver-aware 解析の確信度。互換のため未指定なら出力にも含めない。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub confidence: Option<RefConfidence>,
}

impl RollupRecord for SymbolReference {
    fn rollup_path(&self) -> &str {
        &self.path
    }

    fn rollup_kind(&self) -> Option<&'static str> {
        // `RefKind` の serde 表現 (`def` / `ref`) と揃える。rollup のキーが出力中の
        // `kind` と一致していないと、利用者が省略分と本体を突き合わせられない。
        match self.kind? {
            RefKind::Definition => Some("def"),
            RefKind::Reference => Some("ref"),
        }
    }
}

/// refs のレスポンスエンベロープ。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefsResult {
    pub symbol: String,
    #[serde(rename = "refs")]
    pub references: Vec<SymbolReference>,
    /// Generated files omitted from the directory scan. Missing means zero.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub skipped: Option<SkippedFiles>,
    /// 出力件数の上限で省略が起きたときだけ付く申告。省略が無ければ出力にも現れないため、
    /// 上限に当たらない通常の問い合わせでは従来とバイト単位で同一の出力になる。
    #[serde(
        rename = "result_summary",
        skip_serializing_if = "Option::is_none",
        default
    )]
    pub result_summary: Option<ResultSummary>,
}
