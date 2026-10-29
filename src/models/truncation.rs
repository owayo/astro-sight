use serde::{Deserialize, Serialize};

use super::result_summary::ResultLimitKind;

/// 解析対象を意図的に打ち切った (カバレッジを削った) ことを機械可読に伝える。
///
/// `SkipInfo` が「コマンド全体を解析しなかった」大域的な skip を表すのに対し、
/// `TruncationInfo` は「解析は行ったが一部を対象外にした」部分的な打ち切りを表す。
/// 両者を分けるのは、`skipped` を拡張すると「差分なし / 解析未実行」と誤読され、
/// 打ち切りが起きたのに「全部レビュー済み」と読めてしまうため。
///
/// AGENTS.md のレビュー規約「No silent caps」に対応する: カバレッジを制限したら
/// 何を落としたかを必ず出力に残す。silent truncation は「全部カバーした」と読める。
///
/// 出力契約は **追加のみ** で後方互換: 各結果型に `Vec<TruncationInfo>` として乗り、
/// 空のときは serialize されない (compact 規約)。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TruncationInfo {
    /// 打ち切りの対象パス (`dir` 相対)。対象がファイル単位でない場合は `None`。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub path: Option<String>,
    /// 機械判定用の安定キー。
    pub reason: TruncationReason,
    /// 人間向けの補足メッセージ (閾値と実測値を含める)。
    pub message: String,
    /// 未解析ソースの集約根拠。旧応答や他の理由では省略する。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub detail: Option<UnanalyzableSourceSummary>,
}

/// 拡張子ごとの未解析ファイル数と、上限を設けた代表パス。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UnanalyzableSourceSummary {
    /// 先頭ドットを含まない小文字の拡張子。
    pub extension: String,
    /// 代表パスに切り詰める前の全件数。
    pub count: usize,
    pub examples: Vec<String>,
    /// 出力の上限に収めるために `examples` を縮めたときだけ付く理由 (`token_budget`)。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub examples_limited_by: Option<ResultLimitKind>,
}

/// 打ち切りの理由。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TruncationReason {
    /// 明示 diff の削除前ソースが指定 base に無い、または照合できない。
    DiffBaseMismatch,
    /// 代替文法を含む構文解析の ERROR 内は、宣言・参照・契約変更を検証できない。
    ParseErrorRegion,
    /// 未追跡ファイルが `--git` 合成 diff の取り込み上限を超えたため対象外にした。
    UntrackedFileTooLarge,
    /// ソースコードだがどのバックエンドでも解析できず、走査対象から外れた
    /// (`.vue` / `.svelte` / `.scala` など)。そのファイル内の参照は数えられないため、
    /// dead-code の判定は「参照が無い」ではなく「観測できなかった」可能性を含む。
    UnanalyzableSource,
    /// staged review で index tree のスナップショットが上限を超えたため作らず、
    /// 作業ツリーを読んだ (影響分析の変更ファイルだけは index の blob を読む)。
    StagedSnapshotUnavailable,
    /// 対応言語のソースだが、読み込み (100MB の上限超過・権限など) や構文解析に失敗し、
    /// 中の参照を数えられなかった。dead-code はそのファイルから参照されうる候補を
    /// dead / test_only と断定しない (`dead-code` の出力では `unverified_symbols` に載せる)。
    SourceAnalysisFailed,
}

impl TruncationInfo {
    /// 構造化された集約情報を伴わない打ち切り。
    pub fn new(path: Option<String>, reason: TruncationReason, message: String) -> Self {
        Self {
            path,
            reason,
            message,
            detail: None,
        }
    }

    /// 未追跡ファイルが取り込み上限を超えたため合成 diff に含めなかった打ち切り。
    ///
    /// `limit_label` は超過した上限の種類 (`"size"` / `"lines"`)、`actual` / `limit` は
    /// 実測値と閾値。トリアージ時に「どの閾値にどれだけ超過したか」が分かるようにする。
    pub fn untracked_file_too_large(
        path: &str,
        limit_label: &str,
        actual: usize,
        limit: usize,
    ) -> Self {
        Self::new(
            Some(path.to_string()),
            TruncationReason::UntrackedFileTooLarge,
            format!(
                "untracked file excluded from --git analysis: {limit_label} {actual} exceeds limit {limit}"
            ),
        )
    }

    /// staged review の index tree スナップショットを上限超過で作らなかった打ち切り。
    ///
    /// 件数・バイト数は tree の通常ファイル全体の実測値で、上限と並べて出す。
    /// `working_tree_matches_index` が false なら、作業ツリー由来の参照に依存する API の
    /// 降格を確定させず blocking に戻したことも書く。
    pub fn staged_snapshot_unavailable(
        files: usize,
        bytes: u64,
        file_limit: usize,
        byte_limit: u64,
        working_tree_matches_index: bool,
    ) -> Self {
        let consequence = if working_tree_matches_index {
            "the working tree matched the index, so files were read from the working tree"
        } else {
            "files were read from the working tree, which differs from the index, so \
             reference-based API downgrades (mod_closed / mod_compat / rm_dead) were kept blocking"
        };
        Self::new(
            None,
            TruncationReason::StagedSnapshotUnavailable,
            format!(
                "staged snapshot was not created: {files} files / {bytes} bytes exceed the limit \
                 ({file_limit} files / {byte_limit} bytes); {consequence}"
            ),
        )
    }

    /// 参照を数えられなかったソースファイル (読み込み・構文解析の失敗)。
    /// `message` は失敗の内容と、dead の断定をどこまで控えたかを含める。
    pub fn source_analysis_failed(path: &str, message: String) -> Self {
        Self::new(
            Some(path.to_string()),
            TruncationReason::SourceAnalysisFailed,
            message,
        )
    }

    /// 解析できないソースファイルを拡張子単位で 1 件に畳んだ打ち切り。
    ///
    /// 全件列挙はノイズになり、ピーク RSS を入力件数から独立させる要件にも反するため、
    /// 拡張子ごとに「件数 + 代表パス数件」へ集約する。`examples` は呼び出し側で
    /// ソート済み・件数上限適用済みのものを渡すこと (出力を決定論に保つ)。
    pub fn unanalyzable_source(ext: &str, count: usize, examples: &[String]) -> Self {
        let detail = UnanalyzableSourceSummary {
            extension: ext.to_ascii_lowercase(),
            count,
            examples: examples.to_vec(),
            examples_limited_by: None,
        };
        let mut message = format!(
            "{} \".{}\" file(s) were not analyzed (no parser for this language); \
             references inside them are not counted",
            detail.count, detail.extension
        );
        if !detail.examples.is_empty() {
            message.push_str(&format!(" (e.g. {})", detail.examples.join(", ")));
        }
        Self {
            path: None,
            reason: TruncationReason::UnanalyzableSource,
            message,
            detail: Some(detail),
        }
    }

    /// 代表パス (未解析ソースの `detail.examples`) の件数。代表パスを持たない申告は 0。
    pub fn example_count(&self) -> usize {
        match (&self.reason, &self.detail) {
            (TruncationReason::UnanalyzableSource, Some(detail)) => detail.examples.len(),
            _ => 0,
        }
    }

    /// 代表パスを先頭 `cap` 件までに絞った写し。`message` も絞った代表パスで作り直し、
    /// 実際に縮めたときだけ `limited_by` を `detail.examples_limited_by` に記録する。
    ///
    /// 件数 (`detail.count`) は保つ。代表パスを持たない申告と、`cap` が代表パスの件数
    /// 以上のときはそのまま返す (出力の予算に収めるために標本だけを縮める用途)。
    pub fn with_example_cap(&self, cap: usize, limited_by: ResultLimitKind) -> Self {
        match (&self.reason, &self.detail) {
            (TruncationReason::UnanalyzableSource, Some(detail)) if cap < detail.examples.len() => {
                let mut capped = Self::unanalyzable_source(
                    &detail.extension,
                    detail.count,
                    &detail.examples[..cap],
                );
                if let Some(capped_detail) = capped.detail.as_mut() {
                    capped_detail.examples_limited_by = Some(limited_by);
                }
                capped
            }
            _ => self.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unanalyzable_source_has_structured_detail_and_preserves_message() {
        let examples = vec!["a.vue".to_owned(), "b.vue".to_owned()];
        let info = TruncationInfo::unanalyzable_source("vue", 12, &examples);
        assert_eq!(
            info.message,
            "12 \".vue\" file(s) were not analyzed (no parser for this language); references inside them are not counted (e.g. a.vue, b.vue)"
        );
        let value = serde_json::to_value(&info).unwrap();
        assert_eq!(
            value["detail"],
            serde_json::json!({"extension":"vue", "count":12, "examples":examples})
        );
        assert_eq!(
            serde_json::from_value::<TruncationInfo>(value).unwrap(),
            info
        );
        let old = serde_json::json!({"reason":"unanalyzable_source", "message":"legacy summary"});
        let legacy: TruncationInfo = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(serde_json::to_value(legacy).unwrap(), old);
        let size = TruncationInfo::untracked_file_too_large("generated.rs", "lines", 6000, 5000);
        assert!(serde_json::to_value(size).unwrap().get("detail").is_none());
    }

    /// 代表パスだけを縮め、件数は保ち、`message` は縮めた代表パスで作り直し、
    /// 縮めた理由を `detail.examples_limited_by` に残す。
    #[test]
    fn with_example_cap_shrinks_only_the_examples() {
        let budget = ResultLimitKind::TokenBudget;
        let examples = vec!["a.vue".to_owned(), "b.vue".to_owned(), "c.vue".to_owned()];
        let info = TruncationInfo::unanalyzable_source("vue", 12, &examples);
        assert_eq!(info.example_count(), 3);

        // 対照: 代表パスの件数以上の上限では元のまま (理由も付けない)。
        assert_eq!(info.with_example_cap(3, budget), info);
        assert_eq!(info.with_example_cap(50, budget), info);
        assert!(
            serde_json::to_value(&info).unwrap()["detail"]
                .get("examples_limited_by")
                .is_none()
        );

        let one = info.with_example_cap(1, budget);
        let mut expected = TruncationInfo::unanalyzable_source("vue", 12, &examples[..1]);
        expected.detail.as_mut().unwrap().examples_limited_by = Some(budget);
        assert_eq!(one, expected);
        assert!(one.message.ends_with("(e.g. a.vue)"), "{}", one.message);

        let none = info.with_example_cap(0, budget);
        assert_eq!(
            none.message,
            "12 \".vue\" file(s) were not analyzed (no parser for this language); references inside them are not counted"
        );
        assert_eq!(
            serde_json::to_value(&none).unwrap()["detail"],
            serde_json::json!({
                "extension": "vue",
                "count": 12,
                "examples": [],
                "examples_limited_by": "token_budget"
            })
        );

        // 代表パスを持たない申告 (旧形式・他の理由) はそのまま。
        let legacy: TruncationInfo = serde_json::from_value(
            serde_json::json!({"reason":"unanalyzable_source", "message":"legacy summary"}),
        )
        .unwrap();
        assert_eq!(legacy.example_count(), 0);
        assert_eq!(legacy.with_example_cap(0, budget), legacy);
        let size = TruncationInfo::untracked_file_too_large("generated.rs", "lines", 6000, 5000);
        assert_eq!(size.example_count(), 0);
        assert_eq!(size.with_example_cap(0, budget), size);
    }
}
