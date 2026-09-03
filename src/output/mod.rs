//! 出力フォーマットの選択と直列化。
//!
//! astro-sight の出力面は 2 種類に分かれる。
//!
//! - **データ面** — コマンドの解析結果。`--format` / `config.toml` の `format` で
//!   JSON と TOON を切り替えられる。
//! - **プロトコル面** — `session` の NDJSON、`review --hook` / `impact --hook` の hook
//!   出力、トップレベルのエラー行。いずれも相手側 (Claude Code の Stop hook、既存の
//!   スクリプト、行指向プロトコル) が JSON を前提とする契約のため **常に JSON 固定**。
//!
//! プロトコル面で TOON を要求されたときの扱いは「どこから来た指定か」で変える。
//! CLI で明示的に `--format toon` と書かれた場合はその要求を満たせないので
//! エラーにするが、`config.toml` 由来の既定値は「全コマンドの既定表示形式」を
//! 意味するに過ぎないため、プロトコル面では黙って JSON に倒す。
//! そうしないと `format = "toon"` を設定しただけで `session` や Stop hook が
//! 全滅する (設定として使い物にならない)。

pub mod limit;
mod nullable;
pub mod toon;

use anyhow::Result;
use clap::ValueEnum;
use serde::Deserialize;

use crate::error::{AstroError, ErrorCode};

/// 出力フォーマット。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    // 以下の doc コメントは `--help` にそのまま出るため、他の CLI ヘルプに合わせて英語で書く。
    /// JSON (default)
    #[default]
    Json,
    /// TOON v4.1 - same data, fewer tokens (https://toonformat.dev/)
    Toon,
    /// Whichever of json/toon is estimated to use fewer tokens for this output
    Auto,
}

impl OutputFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            OutputFormat::Json => "json",
            OutputFormat::Toon => "toon",
            OutputFormat::Auto => "auto",
        }
    }
}

/// 1 行あたりの追加コスト (文字数換算)。`SIZE_METRIC` の行罰則項。
///
/// BPE トークナイザでは改行とインデントが 1 行あたりおよそ 1 トークンを消費するため、
/// 素の文字数だけで比べると **行数の多い TOON を過大評価する**。実測 (astro-sight の
/// 出力サンプル × tiktoken `o200k_base` / `cl100k_base`) では、
/// 1 行あたり数文字ぶんの罰則を入れると真のトークン数最小との一致率が上がり、
/// **1 回の呼び出しあたりの最悪損失が 15〜16% から大きく下がる**。
///
/// 値は 2026-09-04 に再測定して 3 から 4 へ引き上げた。`result_summary` (出力上限の申告)
/// という **行数の多い小さな出力**が新たに加わり、旧値の平坦域から外れたため
/// (AGENTS.md の「DTO や出力形を変えたら係数を測り直すこと」に該当する):
///
/// - 大きな出力 143 ペア: k=0..6 がいずれも損失ゼロ (係数の選択が結果を変えない領域)
/// - `result_summary` 付きの小さな出力 120 ペア: k=3 は損失 0.67% / 最悪 93 tokens に対し、
///   **k=4 は損失 0.13% / 最悪 30 tokens**。両トークナイザで k=4 が最良で一致した
///
/// 小さい出力ほど形式間の逆転が起きやすいので、そちらで差が付く値を採る。
const LINE_TOKEN_PENALTY: usize = 4;

/// [`size_metric`] を実トークン数へ換算するときの除数。
///
/// `size_metric` は形式間の**相対比較**用なので係数の絶対値は問わないが、
/// `--token-budget` のような**絶対的な予算**に使うにはトークン数と桁を合わせる必要がある
/// (合わせないと「3000 トークンまで」と指定したのに実際は 900 トークンしか出ない)。
///
/// 実測 (252 サンプル × 2 トークナイザ、`LINE_TOKEN_PENALTY = 4` 時) の `metric / token` は
/// p05=3.00 / p10=3.06 / p50=3.42 / p90=3.86 / min=2.73。**予算は超えない側に倒す**ので
/// 中央値ではなく p05 相当の 3 を採る (95% のケースで予算内、最悪でも約 10% 超過)。
const METRIC_PER_TOKEN: usize = 3;

/// `auto` の比較単位 — トークン数の軽量な推定値。
///
/// 文字数 (Unicode スカラー値の個数) に、改行 1 個あたり `LINE_TOKEN_PENALTY` を足す。
/// バイト長ではなく文字数を基にするのは、日本語 docstring のようなマルチバイト文字が
/// 「1 文字 = 3 バイト」で重み付けされるのを避けるため。
///
/// 実トークナイザを積まないのは、(1) BPE テーブルで数 MB 増える、(2) 消費側のモデルや
/// tokenizer 版で結果が変わり出力の再現性が壊れる、(3) 実測で本推定値は真の最小との差が
/// 全体の 0.0001% 程度しかない、の 3 点による。
///
/// 同点のときは常に JSON 側へ倒す (既定フォーマットで消費側の互換性が最も高いため)。
fn size_metric(text: &str) -> usize {
    let newlines = text.bytes().filter(|b| *b == b'\n').count();
    text.chars().count() + LINE_TOKEN_PENALTY * newlines
}

/// バッチ経路が per-record の集計に使うための公開版。
///
/// これは **形式間の相対比較**用の指標で、単位はトークンではない。絶対的な予算
/// (`--token-budget`) と突き合わせるときは [`estimated_tokens`] を使うこと。
pub fn estimated_size(text: &str) -> usize {
    size_metric(text)
}

/// 出力テキストの推定トークン数。`--token-budget` の判定に使う。
///
/// [`estimated_size`] と違いトークンと同じ桁に揃えてあるので、利用者が指定した
/// 「N トークンまで」がそのまま意味を持つ。切り上げ (`div_ceil`) にするのは、
/// 非空の出力が 0 トークンと見積もられて予算判定が素通りするのを避けるため。
pub fn estimated_tokens(text: &str) -> usize {
    size_metric(text).div_ceil(METRIC_PER_TOKEN)
}

/// JSON の整形スタイル。`--pretty` は JSON 固有の設定で、TOON には対応概念が無い
/// (TOON は元からインデント構造なので compact / pretty の対立が存在しない)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonStyle {
    Compact,
    Pretty,
}

/// 出力設定一式。`--format` / `--pretty` / `config.toml` を解決した結果を
/// commands 層へ渡す。解析ロジック (`AppService` / `models`) には持ち込まない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputOptions {
    format: OutputFormat,
    json_style: JsonStyle,
    /// `--format` が CLI で明示指定されたか。プロトコル面での扱いを分けるために持つ
    /// (モジュール冒頭の説明を参照)。
    explicit_format: bool,
}

impl Default for OutputOptions {
    fn default() -> Self {
        Self {
            format: OutputFormat::Json,
            json_style: JsonStyle::Compact,
            explicit_format: false,
        }
    }
}

impl OutputOptions {
    /// CLI 引数と設定ファイルから出力設定を解決する。
    /// 優先順位は **CLI `--format` > `config.toml` の `format` > json**。
    pub fn resolve(
        cli_format: Option<OutputFormat>,
        config_format: OutputFormat,
        pretty: bool,
    ) -> Self {
        Self {
            format: cli_format.unwrap_or(config_format),
            json_style: if pretty {
                JsonStyle::Pretty
            } else {
                JsonStyle::Compact
            },
            explicit_format: cli_format.is_some(),
        }
    }

    /// テスト・内部利用向けの明示コンストラクタ。
    pub fn new(format: OutputFormat, json_style: JsonStyle) -> Self {
        Self {
            format,
            json_style,
            explicit_format: true,
        }
    }

    pub fn format(&self) -> OutputFormat {
        self.format
    }

    pub fn is_toon(&self) -> bool {
        self.format == OutputFormat::Toon
    }

    pub fn is_auto(&self) -> bool {
        self.format == OutputFormat::Auto
    }

    /// 「1 行 1 レコードの compact JSON をそのまま流せる」状態か。
    /// streaming 経路 (`context` の逐次出力) に乗せてよいかの判定に使う。
    /// `auto` は両形式を比べるまで結果が決まらないので false。
    pub fn streams_compact_json(&self) -> bool {
        self.format == OutputFormat::Json && self.json_style == JsonStyle::Compact
    }

    /// `auto` の判定が済んだ後に、確定したフォーマットで options を作り直す。
    pub fn with_format(self, format: OutputFormat) -> Self {
        Self { format, ..self }
    }

    /// `--pretty` が実際に効く状態か。TOON には整形の概念が無いので常に false。
    /// `calls` のように `--pretty` が DTO 選択も兼ねている既存分岐で使う。
    pub fn is_pretty_json(&self) -> bool {
        self.format == OutputFormat::Json && self.json_style == JsonStyle::Pretty
    }

    /// キャッシュを使ってよいか。ast / symbols のキャッシュは「解析結果」ではなく
    /// **直列化済みの出力そのもの** を保存し、取り出し時に末尾が `}` で閉じることを
    /// truncation 検査に使っている。TOON 出力はこの前提を満たさないため、
    /// JSON compact のときだけキャッシュ経路に乗せる (従来と同じ条件 + format)。
    pub fn cacheable(&self) -> bool {
        self.format == OutputFormat::Json && self.json_style == JsonStyle::Compact
    }

    /// JSON 固定のプロトコル面に入る前の検証。
    ///
    /// CLI で明示された `--format toon` は満たせない要求なのでエラー、config 由来の
    /// 既定値は JSON へ暗黙フォールバックさせる (`Ok`)。
    /// (`auto` はエラーにしない — JSON を選ぶことも `auto` の正当な結果であり、
    /// 「TOON で出せ」という満たせない要求ではないため。)
    ///
    /// **不変条件**: この検証を通す出力面は JSON を直接書く (`serde_json` / `Value` の
    /// `Display`) こと。`serialize_document(value, output)` を通してはならない —
    /// `auto` はここを `Ok` で抜けるため、通すと TOON が選ばれて**黙って**プロトコルが
    /// 壊れる (明示 `toon` と違いエラーで気付けない)。現在の session / hook / エラー行は
    /// いずれも `OutputOptions` を経由せず JSON を直接書いており、
    /// `protocol_surfaces_stay_json_under_auto` がその挙動を固定している。
    pub fn ensure_json_protocol(&self, surface: &str) -> Result<()> {
        if self.format == OutputFormat::Toon && self.explicit_format {
            return Err(AstroError::new(
                ErrorCode::InvalidRequest,
                format!(
                    "--format toon is not supported for {surface}; it emits a line-oriented JSON protocol"
                ),
            )
            .into());
        }
        Ok(())
    }
}

/// 1 ドキュメントを出力形式に従って直列化する。TOON は末尾改行を含まない。
pub fn serialize_document<T: serde::Serialize + ?Sized>(
    value: &T,
    output: OutputOptions,
) -> Result<String> {
    Ok(serialize_document_with_format(value, output)?.0)
}

/// CLI の単一ドキュメントを直列化する。
///
/// JSON は従来の行指向契約を保つため末尾改行を付ける。一方、TOON v4.1 の canonical
/// encoder は末尾改行を禁止しているため、明示指定だけでなく `auto` で TOON が選ばれた
/// 場合も改行を付けない。
pub fn serialize_cli_document<T: serde::Serialize + ?Sized>(
    value: &T,
    output: OutputOptions,
) -> Result<String> {
    let (mut text, selected) = serialize_document_with_format(value, output)?;
    if selected != OutputFormat::Toon {
        text.push('\n');
    }
    Ok(text)
}

fn serialize_document_with_format<T: serde::Serialize + ?Sized>(
    value: &T,
    output: OutputOptions,
) -> Result<(String, OutputFormat)> {
    match output.format {
        OutputFormat::Json => match output.json_style {
            JsonStyle::Compact => Ok((serde_json::to_string(value)?, OutputFormat::Json)),
            JsonStyle::Pretty => Ok((serde_json::to_string_pretty(value)?, OutputFormat::Json)),
        },
        OutputFormat::Toon => Ok((to_toon(value, toon::encode_value)?, OutputFormat::Toon)),
        OutputFormat::Auto => {
            // 比較は常に compact JSON と TOON で行う (どちらも「短く出す」形)。
            // JSON が勝った場合だけ `--pretty` を適用する = auto は形式を選び、
            // `--pretty` は選ばれた JSON の描画方法を決める、という役割分担。
            let compact = serde_json::to_string(value)?;
            let toon = to_toon(value, toon::encode_value)?;
            // 空 object は TOON では「空ドキュメント」= 無出力になる (§8)。仕様上は正しいが、
            // auto で選ぶと「結果が空」と「コマンドが何も出さなかった」を利用者が区別できない。
            // 明示的な `--format toon` は仕様どおりの挙動を維持し、auto だけ JSON へ倒す。
            if !toon.is_empty() && size_metric(&toon) < size_metric(&compact) {
                return Ok((toon, OutputFormat::Toon));
            }
            match output.json_style {
                JsonStyle::Compact => Ok((compact, OutputFormat::Json)),
                JsonStyle::Pretty => Ok((serde_json::to_string_pretty(value)?, OutputFormat::Json)),
            }
        }
    }
}

/// TOON ルート配列の list item 1 件。バッチ (`--paths` / `--dir`) の 1 レコードに使う。
pub fn serialize_toon_list_item<T: serde::Serialize + ?Sized>(value: &T) -> Result<String> {
    to_toon(value, toon::encode_list_item)
}

/// TOON エンコードの共通経路。
///
/// 厳密 (spec どおり) の結果をまず作り、`nullable` の正規化が何かを変えた場合だけ
/// 2 本目を作って **短い方**を採る (同点は厳密側)。これにより
/// 「TOON にしたら JSON より長くなる」という逆効果が構造的に起きない。
/// 正規化が何も変えなければ 2 回目のエンコードは走らない (uniform なデータでは常にこの経路)。
fn to_toon<T, F>(value: &T, encode: F) -> Result<String>
where
    T: serde::Serialize + ?Sized,
    F: Fn(&toon::ToonValue) -> std::result::Result<String, toon::ToonError>,
{
    let to_error = |e: toon::ToonError| -> anyhow::Error {
        AstroError::new(ErrorCode::InternalError, e.to_string()).into()
    };

    let mut toon_value = toon::to_toon_value(value).map_err(to_error)?;
    let strict = encode(&toon_value).map_err(to_error)?;
    if !nullable::fill_optional_columns(&mut toon_value) {
        return Ok(strict);
    }
    let normalized = encode(&toon_value).map_err(to_error)?;
    Ok(if normalized.len() < strict.len() {
        normalized
    } else {
        strict
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Serialize)]
    struct Sample {
        name: String,
        count: usize,
    }

    fn sample() -> Sample {
        Sample {
            name: "foo".into(),
            count: 2,
        }
    }

    #[test]
    fn resolve_prefers_cli_over_config() {
        let opts = OutputOptions::resolve(Some(OutputFormat::Json), OutputFormat::Toon, false);
        assert_eq!(opts.format(), OutputFormat::Json);
        let opts = OutputOptions::resolve(Some(OutputFormat::Toon), OutputFormat::Json, false);
        assert_eq!(opts.format(), OutputFormat::Toon);
    }

    #[test]
    fn resolve_falls_back_to_config_then_json() {
        assert_eq!(
            OutputOptions::resolve(None, OutputFormat::Toon, false).format(),
            OutputFormat::Toon
        );
        assert_eq!(
            OutputOptions::resolve(None, OutputFormat::Json, false).format(),
            OutputFormat::Json
        );
        assert_eq!(OutputOptions::default().format(), OutputFormat::Json);
    }

    #[test]
    fn json_output_is_byte_identical_to_serde_json() {
        // 既存 JSON 出力を一切変えないことがこの変更の前提条件。
        let value = sample();
        let compact = OutputOptions::new(OutputFormat::Json, JsonStyle::Compact);
        assert_eq!(
            serialize_document(&value, compact).unwrap(),
            serde_json::to_string(&value).unwrap()
        );
        let pretty = OutputOptions::new(OutputFormat::Json, JsonStyle::Pretty);
        assert_eq!(
            serialize_document(&value, pretty).unwrap(),
            serde_json::to_string_pretty(&value).unwrap()
        );
    }

    #[test]
    fn toon_output_uses_field_declaration_order() {
        let toon = serialize_document(
            &sample(),
            OutputOptions::new(OutputFormat::Toon, JsonStyle::Compact),
        )
        .unwrap();
        assert_eq!(toon, "name: foo\ncount: 2");
    }

    #[test]
    fn cli_document_uses_a_newline_only_for_selected_json() {
        let json = OutputOptions::new(OutputFormat::Json, JsonStyle::Compact);
        assert_eq!(
            serialize_cli_document(&sample(), json).unwrap(),
            "{\"name\":\"foo\",\"count\":2}\n"
        );

        let toon = OutputOptions::new(OutputFormat::Toon, JsonStyle::Compact);
        assert_eq!(
            serialize_cli_document(&sample(), toon).unwrap(),
            "name: foo\ncount: 2"
        );

        // `auto` でも実際に選ばれた形式に従う。sample は TOON、平坦な短い object は
        // 改行ペナルティにより JSON が勝つため、両分岐を固定する。
        let auto = OutputOptions::new(OutputFormat::Auto, JsonStyle::Compact);
        assert_eq!(
            serialize_cli_document(&sample(), auto).unwrap(),
            "name: foo\ncount: 2"
        );
        let flat = serde_json::json!({ "a": 1, "b": 2, "c": 3, "d": 4 });
        assert_eq!(
            serialize_cli_document(&flat, auto).unwrap(),
            "{\"a\":1,\"b\":2,\"c\":3,\"d\":4}\n"
        );
    }

    #[test]
    fn pretty_is_ignored_for_toon() {
        // TOON には compact / pretty の対立が無い。`--pretty` は JSON 専用設定として
        // 無視する (NDJSON バッチで従来から無視しているのと同じ扱い)。
        let compact = serialize_document(
            &sample(),
            OutputOptions::new(OutputFormat::Toon, JsonStyle::Compact),
        )
        .unwrap();
        let pretty = serialize_document(
            &sample(),
            OutputOptions::new(OutputFormat::Toon, JsonStyle::Pretty),
        )
        .unwrap();
        assert_eq!(compact, pretty);
    }

    #[test]
    fn explicit_toon_is_rejected_on_json_protocol_surfaces() {
        let explicit = OutputOptions::resolve(Some(OutputFormat::Toon), OutputFormat::Json, false);
        assert!(explicit.ensure_json_protocol("session").is_err());
    }

    #[test]
    fn config_sourced_toon_silently_falls_back_on_protocol_surfaces() {
        // `format = "toon"` を設定しただけで session / hook が全滅しないこと。
        let from_config = OutputOptions::resolve(None, OutputFormat::Toon, false);
        assert!(from_config.ensure_json_protocol("session").is_ok());
    }

    #[derive(serde::Serialize)]
    struct Row {
        name: &'static str,
        kind: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        cx: Option<u32>,
    }

    #[derive(serde::Serialize)]
    struct Doc {
        path: &'static str,
        symbols: Vec<Row>,
    }

    fn mixed_doc() -> Doc {
        Doc {
            path: "a.rs",
            symbols: vec![
                Row {
                    name: "MAX",
                    kind: "const",
                    cx: None,
                },
                Row {
                    name: "alpha",
                    kind: "fn",
                    cx: Some(3),
                },
                Row {
                    name: "beta",
                    kind: "fn",
                    cx: Some(1),
                },
            ],
        }
    }

    #[test]
    fn missing_nullable_columns_are_filled_to_reach_tabular_form() {
        // `skip_serializing_if` でキーが欠けた配列も tabular に畳む。畳まないと
        // list form になり JSON より冗長 (= TOON を選ぶ意味が無くなる)。
        let toon = serialize_document(
            &mixed_doc(),
            OutputOptions::new(OutputFormat::Toon, JsonStyle::Compact),
        )
        .unwrap();
        assert_eq!(
            toon,
            "path: a.rs\nsymbols[3]{name,kind,cx}:\n  MAX,const,null\n  alpha,fn,3\n  beta,fn,1"
        );
    }

    #[test]
    fn normalization_is_rejected_when_it_would_be_longer() {
        // 各要素が別々のキーだけを持つと union が広がり、null セルだらけの table が
        // list form より長くなる。実サイズ比較でこれを弾く (never worse の保証)。
        #[derive(serde::Serialize)]
        struct Sparse {
            #[serde(skip_serializing_if = "Option::is_none")]
            alpha_column_name: Option<u32>,
            #[serde(skip_serializing_if = "Option::is_none")]
            bravo_column_name: Option<u32>,
            #[serde(skip_serializing_if = "Option::is_none")]
            delta_column_name: Option<u32>,
        }
        fn row(i: usize) -> Sparse {
            Sparse {
                alpha_column_name: (i == 0).then_some(1),
                bravo_column_name: (i == 1).then_some(2),
                delta_column_name: (i == 2).then_some(3),
            }
        }
        let sparse: Vec<Sparse> = (0..3).map(row).collect();

        let toon = OutputOptions::new(OutputFormat::Toon, JsonStyle::Compact);
        let normalized = serialize_document(&sparse, toon).unwrap();
        let strict = toon::encode(&sparse).unwrap();

        assert_eq!(
            normalized, strict,
            "should keep the strict list form when filling would be longer"
        );
        assert!(normalized.starts_with("[3]:\n  - alpha_column_name: 1"));
    }

    #[test]
    fn auto_picks_the_smaller_estimate() {
        let auto = OutputOptions::new(OutputFormat::Auto, JsonStyle::Compact);
        let json = OutputOptions::new(OutputFormat::Json, JsonStyle::Compact);
        let toon = OutputOptions::new(OutputFormat::Toon, JsonStyle::Compact);

        // object 主体の出力では、引用符と波括弧が消えるぶん TOON が勝つ。
        let doc = mixed_doc();
        assert_eq!(
            serialize_document(&doc, auto).unwrap(),
            serialize_document(&doc, toon).unwrap()
        );

        // プリミティブ配列は JSON の方が短い (`[1,2,3]` < `[3]: 1,2,3`)。
        let flat = vec![1u32, 2, 3];
        assert_eq!(
            serialize_document(&flat, auto).unwrap(),
            serialize_document(&flat, json).unwrap()
        );
    }

    #[test]
    fn auto_never_exceeds_either_candidate() {
        // auto は「推定サイズが小さい方」なので、常に両候補以下になる。
        let auto = OutputOptions::new(OutputFormat::Auto, JsonStyle::Compact);
        let json = OutputOptions::new(OutputFormat::Json, JsonStyle::Compact);
        let toon = OutputOptions::new(OutputFormat::Toon, JsonStyle::Compact);

        for probe in [
            serde_json::json!({"a": 1}),
            serde_json::json!([1, 2, 3]),
            serde_json::json!({"rows": [{"a": 1, "b": 2}, {"a": 3, "b": 4}]}),
            serde_json::json!({"rows": [{"a": 1}, {"b": 2}]}),
            serde_json::json!("plain"),
            serde_json::json!([]),
        ] {
            let n = |o| size_metric(&serialize_document(&probe, o).unwrap());
            assert!(n(auto) <= n(json), "auto worse than json for {probe}");
            assert!(n(auto) <= n(toon), "auto worse than toon for {probe}");
        }
    }

    #[test]
    fn line_penalty_can_flip_the_choice_against_raw_char_count() {
        // 行数の多い TOON は、素の文字数では勝っていても実トークン数では負けうる
        // (BPE では改行 + インデントが 1 行あたり 1 トークンほど掛かる)。
        //
        // このフィクスチャは実トークナイザで裏を取ってある:
        //   json `{"a":1,"b":2,"c":3,"d":4}` = 25 文字 / 17 tokens
        //   toon `a: 1\nb: 2\nc: 3\nd: 4`     = 19 文字 / 19 tokens
        //   (o200k_base / cl100k_base の両方で同じ)
        // 素の文字数で選ぶと TOON を選んで 2 トークン (12%) 損をする。
        let probe = serde_json::json!({"a": 1, "b": 2, "c": 3, "d": 4});
        let auto = OutputOptions::new(OutputFormat::Auto, JsonStyle::Compact);
        let json = OutputOptions::new(OutputFormat::Json, JsonStyle::Compact);
        let toon = OutputOptions::new(OutputFormat::Toon, JsonStyle::Compact);

        let json_text = serialize_document(&probe, json).unwrap();
        let toon_text = serialize_document(&probe, toon).unwrap();
        assert!(
            toon_text.chars().count() < json_text.chars().count(),
            "fixture must favour TOON on raw chars: {json_text:?} / {toon_text:?}"
        );
        assert!(
            size_metric(&toon_text) > size_metric(&json_text),
            "fixture must favour JSON once lines are penalised"
        );
        assert_eq!(serialize_document(&probe, auto).unwrap(), json_text);
    }

    #[test]
    fn size_metric_charges_per_line() {
        // 罰則は改行の個数に比例する (文字数だけの比較との差分を固定する)。
        assert_eq!(size_metric("abc"), 3);
        assert_eq!(size_metric("ab\nc"), 4 + LINE_TOKEN_PENALTY);
        assert_eq!(size_metric("a\nb\nc"), 5 + 2 * LINE_TOKEN_PENALTY);
        // マルチバイト文字は 1 文字として数える (バイト長では重み付けしない)。
        assert_eq!(size_metric("日本語"), 3);
    }

    /// `estimated_tokens` は実トークン数と同じ桁でなければならない。
    ///
    /// `size_metric` は形式間の**相対比較**用なので係数の絶対値は問わないが、
    /// `--token-budget` は利用者が「N トークンまで」と指定する**絶対値**。桁が
    /// ずれると「3000 と指定したのに 900 しか出ない」という乖離になる (実際に
    /// 一度そうなっていた)。実測 (252 サンプル × 2 トークナイザ) の metric/token は
    /// p05=3.00 / p50=3.42 で、除数 3 は「予算を超えない側」に倒した値。
    #[test]
    fn estimated_tokens_is_metric_scaled_to_token_units() {
        // 換算は metric / METRIC_PER_TOKEN の切り上げ
        assert_eq!(estimated_tokens("abc"), 1);
        assert_eq!(estimated_tokens(&"x".repeat(300)), 100);
        assert_eq!(estimated_tokens(&"x".repeat(301)), 101);
        // 非空の出力が 0 トークンと見積もられて予算判定が素通りしない
        assert_eq!(estimated_tokens("a"), 1);
        assert_eq!(estimated_tokens(""), 0);
        // 改行の罰則は metric 側と同じ扱い (行の多い出力を過小評価しない)
        let lines = "ab
"
        .repeat(30);
        assert_eq!(
            estimated_tokens(&lines),
            size_metric(&lines).div_ceil(METRIC_PER_TOKEN)
        );
        assert!(
            estimated_tokens(&lines) > lines.chars().count().div_ceil(METRIC_PER_TOKEN),
            "改行ぶんの罰則が乗る"
        );
    }

    #[test]
    fn auto_does_not_choose_an_empty_toon_document() {
        // 空 object は TOON では無出力になる。auto がそれを選ぶと「結果が空」と
        // 「何も出力されなかった」を区別できなくなるため JSON へ倒す。
        #[derive(serde::Serialize)]
        struct Empty {}
        let auto = OutputOptions::new(OutputFormat::Auto, JsonStyle::Compact);
        assert_eq!(serialize_document(&Empty {}, auto).unwrap(), "{}");

        // 明示的な `--format toon` は仕様どおり空ドキュメントのまま。
        let toon = OutputOptions::new(OutputFormat::Toon, JsonStyle::Compact);
        assert_eq!(serialize_document(&Empty {}, toon).unwrap(), "");
    }

    #[test]
    fn auto_applies_pretty_only_when_json_wins() {
        // auto は「形式」を選び、`--pretty` は選ばれた JSON の描画方法を決める。
        // 比較そのものは常に compact JSON と TOON で行う。
        let auto_pretty = OutputOptions::new(OutputFormat::Auto, JsonStyle::Pretty);

        let flat = vec![1u32, 2, 3];
        assert_eq!(
            serialize_document(&flat, auto_pretty).unwrap(),
            serde_json::to_string_pretty(&flat).unwrap()
        );

        // TOON が勝つ入力では `--pretty` は効かない (TOON に整形の概念が無いため)。
        let doc = mixed_doc();
        assert_eq!(
            serialize_document(&doc, auto_pretty).unwrap(),
            serialize_document(
                &doc,
                OutputOptions::new(OutputFormat::Toon, JsonStyle::Compact)
            )
            .unwrap()
        );
    }

    #[test]
    fn auto_is_allowed_on_json_protocol_surfaces() {
        // `auto` は「TOON で出せ」という要求ではないので、プロトコル面でもエラーにしない
        // (JSON を選ぶことも auto の正当な結果)。
        let explicit = OutputOptions::resolve(Some(OutputFormat::Auto), OutputFormat::Json, false);
        assert!(explicit.ensure_json_protocol("session").is_ok());
    }

    #[test]
    fn only_compact_json_is_cacheable() {
        assert!(OutputOptions::new(OutputFormat::Json, JsonStyle::Compact).cacheable());
        assert!(!OutputOptions::new(OutputFormat::Json, JsonStyle::Pretty).cacheable());
        assert!(!OutputOptions::new(OutputFormat::Toon, JsonStyle::Compact).cacheable());
        // auto は内容によって出力形式が変わるうえ、キャッシュの truncation 検査が
        // 末尾 `}` 前提なので対象外。
        assert!(!OutputOptions::new(OutputFormat::Auto, JsonStyle::Compact).cacheable());
    }
}
