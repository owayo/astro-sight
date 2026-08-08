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
    /// Whichever of json/toon produces fewer characters for this output
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

/// `auto` の比較単位。文字数 (Unicode スカラー値の個数) で比べる。
///
/// バイト長ではなく文字数にするのは、日本語 docstring のようなマルチバイト文字が
/// 「1 文字 = 3 バイト」で重み付けされるのを避けるため。両形式が運ぶ本文は同一なので
/// 実際には勝敗が変わることはまずないが、「出力文字数が少ない方」という規則を
/// そのまま実装しておく方が説明と一致する。
/// 同点のときは常に JSON 側へ倒す (既定フォーマットで消費側の互換性が最も高いため)。
fn char_len(text: &str) -> usize {
    text.chars().count()
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
    match output.format {
        OutputFormat::Json => match output.json_style {
            JsonStyle::Compact => Ok(serde_json::to_string(value)?),
            JsonStyle::Pretty => Ok(serde_json::to_string_pretty(value)?),
        },
        OutputFormat::Toon => to_toon(value, toon::encode_value),
        OutputFormat::Auto => {
            // 比較は常に compact JSON と TOON で行う (どちらも「短く出す」形)。
            // JSON が勝った場合だけ `--pretty` を適用する = auto は形式を選び、
            // `--pretty` は選ばれた JSON の描画方法を決める、という役割分担。
            let compact = serde_json::to_string(value)?;
            let toon = to_toon(value, toon::encode_value)?;
            // 空 object は TOON では「空ドキュメント」= 無出力になる (§8)。仕様上は正しいが、
            // auto で選ぶと「結果が空」と「コマンドが何も出さなかった」を利用者が区別できない。
            // 明示的な `--format toon` は仕様どおりの挙動を維持し、auto だけ JSON へ倒す。
            if !toon.is_empty() && char_len(&toon) < char_len(&compact) {
                return Ok(toon);
            }
            match output.json_style {
                JsonStyle::Compact => Ok(compact),
                JsonStyle::Pretty => Ok(serde_json::to_string_pretty(value)?),
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
    fn auto_picks_the_shorter_of_json_and_toon() {
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
        // auto は「短い方」なので、常に両候補以下になる。
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
            let n = |o| serialize_document(&probe, o).unwrap().chars().count();
            assert!(n(auto) <= n(json), "auto longer than json for {probe}");
            assert!(n(auto) <= n(toon), "auto longer than toon for {probe}");
        }
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
