use serde::{Deserialize, Serialize};

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
}

/// 打ち切りの理由。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TruncationReason {
    /// 未追跡ファイルが `--git` 合成 diff の取り込み上限を超えたため対象外にした。
    UntrackedFileTooLarge,
    /// ソースコードだがどのバックエンドでも解析できず、走査対象から外れた
    /// (`.vue` / `.svelte` / `.scala` など)。そのファイル内の参照は数えられないため、
    /// dead-code の判定は「参照が無い」ではなく「観測できなかった」可能性を含む。
    UnanalyzableSource,
}

impl TruncationInfo {
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
        Self {
            path: Some(path.to_string()),
            reason: TruncationReason::UntrackedFileTooLarge,
            message: format!(
                "untracked file excluded from --git analysis: {limit_label} {actual} exceeds limit {limit}"
            ),
        }
    }

    /// 解析できないソースファイルを拡張子単位で 1 件に畳んだ打ち切り。
    ///
    /// 全件列挙はノイズになり、ピーク RSS を入力件数から独立させる要件にも反するため、
    /// 拡張子ごとに「件数 + 代表パス数件」へ集約する。`examples` は呼び出し側で
    /// ソート済み・件数上限適用済みのものを渡すこと (出力を決定論に保つ)。
    pub fn unanalyzable_source(ext: &str, count: usize, examples: &[String]) -> Self {
        let mut message = format!(
            "{count} \".{ext}\" file(s) were not analyzed (no parser for this language); \
             references inside them are not counted"
        );
        if !examples.is_empty() {
            message.push_str(&format!(" (e.g. {})", examples.join(", ")));
        }
        Self {
            path: None,
            reason: TruncationReason::UnanalyzableSource,
            message,
        }
    }
}
