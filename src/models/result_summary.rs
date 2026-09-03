//! 出力件数の上限 (result-set management) と、その申告用サマリ。
//!
//! `refs` のような「1 回の呼び出しで数千件返りうる」コマンドは、表現 (キー短縮 / TOON) を
//! いくら最適化しても件数が無防備だと 1 回で数万トークンを消費する (実測: 自リポジトリの
//! `refs --name new --dir .` が 1,723 件 / 約 37,000 tokens)。エージェントは呼ぶ前に
//! その識別子が高頻度だと知りようがないため、`--glob` の併用を促すだけでは解決しない。
//!
//! 本モジュールは **表現の問題ではなく result-set management の欠落**として、上限と
//! 「何をどれだけ省略したか」の申告を独立した概念で持つ。設計上の要点:
//!
//! - **解析は止めない**。全件解析して `total` を正確に出し、**出力だけ**を絞る。
//!   N 件で走査を打ち切ると (a) 正確な `total` が取れない (b) rollup を作れない
//!   (c) 後半のエラーを見落とす、という 3 つの損失が同時に起きる。
//! - **cap は表現層だけに効く**。dead-code / api 差分 / hook の判定はこれまでどおり
//!   全件を見た内部 API の結果で行う (件数制限と意味解析を混ぜない)。
//! - **省略が 1 件でも起きたときだけ** `result_summary` を出す。起きなければ出力は
//!   従来とバイト単位で同一 = 既存 consumer の契約を壊さない。
//! - `by_kind` / `files` は **省略された分だけ**の分布。本体込みの総分布にすると
//!   利用者が「省略分」を引き算で復元できない。
//! - rollup 自身も無制限にしない。`files` は上位 [`MAX_ROLLUP_FILES`] 件までで、
//!   残りは [`OmittedOtherFiles`] へ畳む (第二の出力爆発を作らない)。
//!
//! [`TruncationInfo`](crate::models::truncation::TruncationInfo) とは別概念である点に注意。
//! あちらは **入力側**の打ち切り (解析対象から外したファイル)、こちらは **出力側**の
//! cap (解析はしたが表示しなかった件数)。混ぜると「レビュー済み」と「表示済み」を
//! 利用者が区別できなくなる。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// `files` rollup に載せる最大件数。残りは [`OmittedOtherFiles`] へ畳む。
pub const MAX_ROLLUP_FILES: usize = 12;

/// `--max-results` の既定値。
///
/// 自リポジトリの全シンボルから 400 個を無作為抽出して `refs` を実行した実測分布は
/// p50=2 / p75=4 / p90=14 / p95=24 / p98=58 / p99=191 / max=1139 で、100 件を超えるのは
/// 1.5% (6/400) だけだった。つまりこの既定値は通常の問い合わせには当たらず、
/// トークン爆発を起こす長い裾だけを止める。
pub const DEFAULT_MAX_RESULTS: usize = 100;

/// `--token-budget` の既定値 (推定トークン数)。
pub const DEFAULT_TOKEN_BUDGET: usize = 3_000;

/// `--token-budget` に指定できる下限。
///
/// これを下回るとサマリ自体すら収まらず、「0 件 + 収まらないサマリ」しか返せなくなる。
/// 黙って破綻させるより CLI 引数エラーで弾く。
pub const MIN_TOKEN_BUDGET: usize = 256;

/// 無制限を表す CLI / session の指定値。
pub const UNLIMITED_KEYWORD: &str = "unlimited";

/// `--max-results` / `--token-budget` の文字列指定を解釈する。
///
/// `unlimited` は `None` (無制限)、それ以外は非負整数。`min` を下回る値は拒否する
/// (予算がサマリすら収まらない設定を黙って受けない)。
pub fn parse_limit_arg(raw: &str, flag: &str, min: usize) -> Result<Option<usize>, String> {
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case(UNLIMITED_KEYWORD) {
        return Ok(None);
    }
    let value: usize = trimmed
        .parse()
        .map_err(|_| format!("{flag} must be a non-negative integer or `{UNLIMITED_KEYWORD}`"))?;
    if value < min {
        return Err(format!(
            "{flag} must be >= {min} (or `{UNLIMITED_KEYWORD}`)"
        ));
    }
    Ok(Some(value))
}

/// JSON API (session / MCP) での上限指定。
///
/// CLI の `N|unlimited` と等価な表現。`100` のような数値か、文字列 `"unlimited"` を受ける。
/// 省略 (`null` / フィールド無し) は既定値の適用を意味する。
///
/// `Option<usize>` で「省略 = 無制限」にすると、既定値を効かせたい呼び出しと無制限に
/// したい呼び出しを区別できない。負値をセンチネルにする案は `usize` の型と噛み合わず、
/// スキーマ上も意図が読めないため採らない。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum LimitValue {
    Count(usize),
    Keyword(String),
}

impl LimitValue {
    /// 上限値へ解決する。`None` は無制限。不正な文字列はエラー。
    pub fn resolve(&self, flag: &str, min: usize) -> Result<Option<usize>, String> {
        match self {
            Self::Count(n) => {
                if *n < min {
                    return Err(format!(
                        "{flag} must be >= {min} (or `{UNLIMITED_KEYWORD}`)"
                    ));
                }
                Ok(Some(*n))
            }
            Self::Keyword(s) => parse_limit_arg(s, flag, min),
        }
    }
}

/// session / MCP の上限指定を [`ResultLimits`] へ解決する。省略時は既定値。
pub fn resolve_limits(
    max_results: Option<&LimitValue>,
    token_budget: Option<&LimitValue>,
) -> Result<ResultLimits, String> {
    let max_results = match max_results {
        Some(v) => v.resolve("max_results", 0)?,
        None => Some(DEFAULT_MAX_RESULTS),
    };
    let token_budget = match token_budget {
        Some(v) => v.resolve("token_budget", MIN_TOKEN_BUDGET)?,
        None => Some(DEFAULT_TOKEN_BUDGET),
    };
    Ok(ResultLimits {
        max_results,
        token_budget,
    })
}

/// 出力を絞った理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ResultLimitKind {
    /// `--max-results` に達した。
    #[serde(rename = "max_results")]
    MaxResults,
    /// `--token-budget` に達した。
    #[serde(rename = "token_budget")]
    TokenBudget,
}

/// 実際に適用された上限値。`None` は「無制限」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultLimits {
    #[serde(rename = "max_results", skip_serializing_if = "Option::is_none")]
    pub max_results: Option<usize>,
    #[serde(rename = "token_budget", skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<usize>,
}

impl ResultLimits {
    /// 上限を一切課さない設定。
    pub const UNLIMITED: Self = Self {
        max_results: None,
        token_budget: None,
    };

    /// 既定の上限。
    pub const DEFAULT: Self = Self {
        max_results: Some(DEFAULT_MAX_RESULTS),
        token_budget: Some(DEFAULT_TOKEN_BUDGET),
    };

    /// 上限が 1 つも無い (= 絞らない) か。
    pub fn is_unlimited(&self) -> bool {
        self.max_results.is_none() && self.token_budget.is_none()
    }
}

impl Default for ResultLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// 省略された分のファイル別内訳 1 件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OmittedFileRollup {
    pub path: String,
    pub count: usize,
}

/// `files` に載せきれなかった残りファイルの合計。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OmittedOtherFiles {
    pub files: usize,
    pub count: usize,
}

/// rollup 自体を打ち切ったことの申告。
///
/// 「省略分の内訳」を出す機能が、それ自身の上限で更に省略されたことを黙らせないために持つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollupTruncation {
    pub shown: usize,
    pub available: usize,
}

/// 出力を絞ったときの申告。省略が 0 件なら生成しない。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultSummary {
    /// 実際に出力したレコード数。
    pub shown: usize,
    /// 解析できた入力集合に対する総件数。`complete_input` が false ならリポジトリ全体の
    /// 真の総数ではない点に注意。
    pub total: usize,
    /// `total - shown`。
    pub omitted: usize,
    /// どの上限で絞られたか (複数同時に成立しうる)。昇順で固定。
    pub limited_by: Vec<ResultLimitKind>,
    /// 適用された上限値。
    pub limits: ResultLimits,
    /// `total` が「解析対象にできた入力すべて」を数えているか。
    /// 入力側の打ち切り・読み込み失敗・parse 失敗があれば false。
    pub complete_input: bool,
    /// **省略された分だけ**の kind 別件数。キー昇順で決定的。
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub by_kind: BTreeMap<String, usize>,
    /// **省略された分だけ**の言語別件数。キー昇順で決定的。
    ///
    /// polyglot リポジトリでは bare name の一致が言語をまたいで大量に出る
    /// (実測: 名前 `search` の参照 2,522 件のうち 2,521 件が PHP / JS / C / TS 等の
    /// 別言語で、探していた Python の定義へ到達する経路は 1 件も無かった)。
    /// 省略分の言語構成が見えれば `--glob` で絞り直す判断ができる。
    /// 拡張子から言語を決められないファイルは数えない (キーを増やさない)。
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub by_lang: BTreeMap<String, usize>,
    /// **省略された分だけ**のファイル別件数 (件数降順 → パス昇順、上位 [`MAX_ROLLUP_FILES`] 件)。
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub files: Vec<OmittedFileRollup>,
    /// `files` に載らなかった残り。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub other_files: Option<OmittedOtherFiles>,
    /// `files` 自体を打ち切った場合の申告。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub rollup_truncated: Option<RollupTruncation>,
}

/// rollup の材料になれるレコード。
pub trait RollupRecord {
    /// ファイル別 rollup のキー。`--dir` 相対パスを想定する。
    fn rollup_path(&self) -> &str;
    /// kind 別 rollup のキー。分類が無いレコードは `None`。
    fn rollup_kind(&self) -> Option<&'static str>;
}

/// rollup 1 件が出力に占めるおおよその文字数 (`{"path":"...","count":N}` の固定部)。
/// 予算からファイル件数を逆算するための見積もりに使う。
const ROLLUP_ENTRY_OVERHEAD: usize = 24;

/// rollup に割り当てる予算の割合 (分母)。予算全体の 1/10。
const ROLLUP_BUDGET_DIVISOR: usize = 10;

/// 文字数からトークン数への概算比。
///
/// [`crate::output::estimated_tokens`] が使う換算と同じ規約 (実測 p05 ≈ 3.0 文字/トークン)。
/// 予算 (トークン) と突き合わせる前にエントリのコストを揃えるために使う。厳密な予算判定は
/// 表現層が実テキストに対して行うので、ここでは件数の逆算に足る粗さで十分。
const APPROX_CHARS_PER_TOKEN: usize = 3;

/// トークン予算から rollup に載せられるファイル件数を求める。
///
/// サマリは「省略分の在り処」を示すためのものなので、それ自身が予算を食い潰すと
/// 本体が出せなくなる (実測: 12 件の rollup で予算 3,000 のうち約 900 を消費し、
/// 出力できる参照が 17 件まで落ちた)。予算の 1/10 を上限として、実際のパス長から
/// 件数を逆算する。削った事実は [`RollupTruncation`] で申告する。
///
/// **予算とコストの単位はどちらもトークンに揃える。** `token_budget` はトークン数、
/// パス長は文字数なので、揃えないと rollup が実際の 1/3 に絞られる。
///
/// 予算が無い (無制限) 場合は [`MAX_ROLLUP_FILES`] をそのまま使う。
/// 最低 1 件は載せる — 0 件だと「省略分がどこにあるか」の手掛かりが完全に消え、
/// `--glob` で絞り直す導線が無くなるため。
fn rollup_files_for_budget(token_budget: Option<usize>, ranked_paths: &[(&str, usize)]) -> usize {
    let Some(budget) = token_budget else {
        return MAX_ROLLUP_FILES;
    };
    let mut remaining = budget / ROLLUP_BUDGET_DIVISOR;
    let mut count = 0usize;
    for (path, _) in ranked_paths.iter().take(MAX_ROLLUP_FILES) {
        let cost = (path.chars().count() + ROLLUP_ENTRY_OVERHEAD).div_ceil(APPROX_CHARS_PER_TOKEN);
        if cost > remaining {
            break;
        }
        remaining -= cost;
        count += 1;
    }
    count.max(1)
}

/// 省略された `omitted` から rollup を組み立てる。
///
/// 並び順は **件数降順 → パス昇順** で固定する。`HashMap` の反復順や入力順が
/// ユーザー可視の配列に漏れないよう、最後に必ず全順序でソートする。
///
/// `token_budget` は「サマリ自身が予算を食い潰さない」ための上限計算に使う
/// (`None` なら [`MAX_ROLLUP_FILES`] 固定)。超過分は [`OmittedOtherFiles`] へ畳み、
/// 削ったこと自体を [`RollupTruncation`] で申告する。
pub fn build_rollup<T: RollupRecord>(
    omitted: &[T],
    token_budget: Option<usize>,
) -> RollupBreakdown {
    let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_lang: BTreeMap<String, usize> = BTreeMap::new();
    let mut per_file: BTreeMap<&str, usize> = BTreeMap::new();
    // 言語判定はパス単位で 1 回だけ行う (省略分は数千件になりうるので、
    // レコードごとに拡張子を引き直さない)。
    let mut lang_of_path: BTreeMap<&str, Option<&'static str>> = BTreeMap::new();
    for record in omitted {
        if let Some(kind) = record.rollup_kind() {
            *by_kind.entry(kind.to_string()).or_insert(0) += 1;
        }
        let path = record.rollup_path();
        let lang = *lang_of_path
            .entry(path)
            .or_insert_with(|| lang_name_for_path(path));
        if let Some(lang) = lang {
            *by_lang.entry(lang.to_string()).or_insert(0) += 1;
        }
        *per_file.entry(path).or_insert(0) += 1;
    }

    // BTreeMap はパス昇順なので、件数降順で stable sort すれば
    // 「件数降順 → パス昇順」の全順序になる。
    let mut ranked: Vec<(&str, usize)> = per_file.into_iter().collect();
    ranked.sort_by_key(|a| std::cmp::Reverse(a.1));

    let available = ranked.len();
    let shown = available.min(rollup_files_for_budget(token_budget, &ranked));
    let files: Vec<OmittedFileRollup> = ranked[..shown]
        .iter()
        .map(|(path, count)| OmittedFileRollup {
            path: (*path).to_string(),
            count: *count,
        })
        .collect();

    let (other_files, rollup_truncated) = if available > shown {
        let rest = &ranked[shown..];
        (
            Some(OmittedOtherFiles {
                files: rest.len(),
                count: rest.iter().map(|(_, c)| *c).sum(),
            }),
            Some(RollupTruncation { shown, available }),
        )
    } else {
        (None, None)
    };

    RollupBreakdown {
        by_kind,
        by_lang,
        files,
        other_files,
        rollup_truncated,
    }
}

/// パスの拡張子から言語名を引く。決められないものは `None` (キーを増やさない)。
///
/// 名前は他の出力の `lang` フィールドと同じ語彙 (`rust` / `javascript` / `csharp`)。
/// 拡張子だけで決めるので shebang しか手掛かりのないファイル (拡張子なしの Python /
/// Bash スクリプト) は数えない — rollup は「省略した参照の分布」を示すためのもので、
/// ファイルを読み直してまで精度を上げる価値はない。
fn lang_name_for_path(path: &str) -> Option<&'static str> {
    let lang = crate::language::LangId::from_path(camino::Utf8Path::new(path)).ok()?;
    Some(lang.detected().display_name())
}

/// [`build_rollup`] の結果。位置引数のタプルで返すと、同型の `BTreeMap` が 2 つ
/// (`by_kind` / `by_lang`) 並ぶため取り違えてもコンパイルが通る。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RollupBreakdown {
    pub by_kind: BTreeMap<String, usize>,
    pub by_lang: BTreeMap<String, usize>,
    pub files: Vec<OmittedFileRollup>,
    pub other_files: Option<OmittedOtherFiles>,
    pub rollup_truncated: Option<RollupTruncation>,
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rec(&'static str, Option<&'static str>);

    impl RollupRecord for Rec {
        fn rollup_path(&self) -> &str {
            self.0
        }
        fn rollup_kind(&self) -> Option<&'static str> {
            self.1
        }
    }

    /// rollup は件数降順 → パス昇順で並ぶ (同数のときに入力順が漏れない)。
    #[test]
    fn rollup_orders_by_count_then_path() {
        let omitted = vec![
            Rec("z.rs", Some("ref")),
            Rec("a.rs", Some("ref")),
            Rec("z.rs", Some("def")),
            Rec("m.rs", Some("ref")),
            Rec("a.rs", Some("ref")),
        ];
        let RollupBreakdown {
            by_kind,
            files,
            other_files: other,
            rollup_truncated: trunc,
            ..
        } = build_rollup(&omitted, None);
        assert_eq!(by_kind.get("ref"), Some(&4));
        assert_eq!(by_kind.get("def"), Some(&1));
        // a.rs=2 / z.rs=2 / m.rs=1 → 件数降順、同数はパス昇順で a.rs が先
        assert_eq!(
            files
                .iter()
                .map(|f| (f.path.as_str(), f.count))
                .collect::<Vec<_>>(),
            vec![("a.rs", 2), ("z.rs", 2), ("m.rs", 1)]
        );
        assert!(other.is_none());
        assert!(trunc.is_none());
    }

    /// rollup 自身も上限を持ち、超過分は other_files に畳んで申告する。
    #[test]
    fn rollup_caps_files_and_declares_truncation() {
        // 20 ファイル × 1 件。件数が同じなのでパス昇順が全順序を決める。
        let paths: Vec<String> = (0..20).map(|i| format!("f{i:02}.rs")).collect();
        let omitted: Vec<Rec> = paths
            .iter()
            .map(|p| Rec(Box::leak(p.clone().into_boxed_str()), Some("ref")))
            .collect();
        let RollupBreakdown {
            files,
            other_files: other,
            rollup_truncated: trunc,
            ..
        } = build_rollup(&omitted, None);
        assert_eq!(files.len(), MAX_ROLLUP_FILES);
        assert_eq!(files[0].path, "f00.rs");
        assert_eq!(files[MAX_ROLLUP_FILES - 1].path, "f11.rs");
        assert_eq!(
            other,
            Some(OmittedOtherFiles {
                files: 20 - MAX_ROLLUP_FILES,
                count: 20 - MAX_ROLLUP_FILES
            })
        );
        assert_eq!(
            trunc,
            Some(RollupTruncation {
                shown: MAX_ROLLUP_FILES,
                available: 20
            })
        );
    }

    /// 省略分の言語別内訳を出す。polyglot リポジトリで「同名の別言語シンボルに
    /// 埋もれている」ことを利用者が判断できるようにするため。
    #[test]
    fn rollup_counts_omitted_by_language() {
        let omitted = vec![
            Rec("src/a.rs", Some("ref")),
            Rec("src/b.rs", Some("ref")),
            Rec("web/app.ts", Some("ref")),
            Rec("legacy/x.php", Some("ref")),
            Rec("legacy/y.php", Some("ref")),
            Rec("legacy/z.php", Some("ref")),
            // 拡張子から言語を決められないものは数えない (キーを増やさない)
            Rec("Makefile", Some("ref")),
            Rec("data.bin", Some("ref")),
        ];
        let rollup = build_rollup(&omitted, None);
        assert_eq!(rollup.by_lang.get("php"), Some(&3));
        assert_eq!(rollup.by_lang.get("rust"), Some(&2));
        assert_eq!(rollup.by_lang.get("typescript"), Some(&1));
        assert_eq!(
            rollup.by_lang.len(),
            3,
            "言語不明のファイルはキーを増やさない: {:?}",
            rollup.by_lang
        );
        // ファイル別 rollup は言語に関係なく全件数える (対照)
        assert_eq!(
            rollup.files.iter().map(|f| f.count).sum::<usize>(),
            omitted.len()
        );
    }

    /// 空の省略集合では rollup も空になる (呼び出し側が summary を作らないことの前提)。
    #[test]
    fn rollup_of_empty_omitted_is_empty() {
        let omitted: Vec<Rec> = Vec::new();
        let RollupBreakdown {
            by_kind,
            files,
            other_files: other,
            rollup_truncated: trunc,
            ..
        } = build_rollup(&omitted, None);
        assert!(by_kind.is_empty());
        assert!(files.is_empty());
        assert!(other.is_none());
        assert!(trunc.is_none());
    }
}
