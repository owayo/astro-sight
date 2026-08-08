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

pub mod toon;

use anyhow::Result;
use clap::ValueEnum;
use serde::Deserialize;

use crate::error::{AstroError, ErrorCode};

/// 出力フォーマット。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    /// JSON (既定)。
    #[default]
    Json,
    /// TOON v4.1。
    Toon,
}

impl OutputFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            OutputFormat::Json => "json",
            OutputFormat::Toon => "toon",
        }
    }
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
        OutputFormat::Toon => toon::encode(value)
            .map_err(|e| AstroError::new(ErrorCode::InternalError, e.to_string()).into()),
    }
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

    #[test]
    fn only_compact_json_is_cacheable() {
        assert!(OutputOptions::new(OutputFormat::Json, JsonStyle::Compact).cacheable());
        assert!(!OutputOptions::new(OutputFormat::Json, JsonStyle::Pretty).cacheable());
        assert!(!OutputOptions::new(OutputFormat::Toon, JsonStyle::Compact).cacheable());
    }
}
