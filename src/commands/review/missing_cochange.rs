//! `review` の missing_cochange 警告生成。
//!
//! 「同時に変更されるべきファイルが diff に含まれていない」ことの検出であり、
//! API 差分検出 (`api_changes`) ではなく review 側の責務のためここに置く。

use anyhow::Result;
use std::collections::HashSet;

use crate::models::cochange::CoChangeOptions;
use crate::models::review::MissingCochange;
use crate::service::AppService;

/// 依存マニフェストとロックファイルの既知ペア。
/// これらは `cargo update` や `npm install` など片側のみが変更される正規操作が頻繁に発生するため、
/// missing_cochange 警告から除外する。同一ディレクトリに属するペアのみ除外対象とする（monorepo 配慮）。
pub(crate) const DEPENDENCY_MANIFEST_LOCK_PAIRS: &[(&str, &str)] = &[
    ("Cargo.toml", "Cargo.lock"),
    ("package.json", "package-lock.json"),
    ("package.json", "pnpm-lock.yaml"),
    ("package.json", "yarn.lock"),
    ("pyproject.toml", "uv.lock"),
    ("pyproject.toml", "poetry.lock"),
    ("pyproject.toml", "pdm.lock"),
    ("Gemfile", "Gemfile.lock"),
    ("composer.json", "composer.lock"),
    ("go.mod", "go.sum"),
    ("mix.exs", "mix.lock"),
];

/// 2 つのパスが既知の依存マニフェスト/ロックペアであれば true を返す。
/// monorepo 誤判定を避けるため、親ディレクトリが一致する場合のみ真。
pub(crate) fn is_dependency_manifest_pair(file_a: &str, file_b: &str) -> bool {
    let path_a = std::path::Path::new(file_a);
    let path_b = std::path::Path::new(file_b);
    let (Some(base_a), Some(base_b)) = (
        path_a.file_name().and_then(|s| s.to_str()),
        path_b.file_name().and_then(|s| s.to_str()),
    ) else {
        return false;
    };
    if path_a.parent() != path_b.parent() {
        return false;
    }
    DEPENDENCY_MANIFEST_LOCK_PAIRS
        .iter()
        .any(|(a, b)| (base_a == *a && base_b == *b) || (base_a == *b && base_b == *a))
}

/// review の missing_cochanges が要求する既定の最小共変更回数。
///
/// standalone の `cochange` は探索的な履歴分析なので既定 2 のままにし、review だけ 3 を
/// 要求する。confidence は raw の `co / 実効分母` なので、変更行 blame で分母が 2 しか
/// 作れない起点では「1 回だけ一緒に変わった」ペアが co=2/denom=2 = confidence 1.0 として
/// 最上位に並ぶ。review は「その変更で直し忘れている相方」を出す場所で、履歴 1〜2 回の
/// 相関を必須共変更として提示すると毎回同じ FP が出てトリアージが空振りする
/// (実測: 実リポジトリの missing_cochanges 6 件がすべて confidence 1.0 の FP)。
///
/// 閾値を smoothed `score` 側へ移さないのは意図的。score は分母が小さいほど 0 に
/// 引き寄せられる shrinkage 推定値で、既定 β=8 では分母 2 の上限が 0.27 となり
/// 「変更行 blame の起点は 100% 共変更でも構造的に出力不能」という以前の穴に戻る。
/// support の要求は分子 (co_changes) の下限で表す。
pub(crate) const REVIEW_COCHANGE_MIN_SAMPLES: usize = 3;

/// `detect_missing_cochanges` の結果。0 件の理由を呼び出し側 (review) に伝えるため、
/// 検出結果と解析の内訳を一緒に返す。
#[derive(Debug)]
pub(crate) struct MissingCochangeReport {
    pub(crate) missing: Vec<MissingCochange>,
    pub(crate) diagnostics: crate::models::cochange::CoChangeDiagnostics,
}

pub(crate) fn detect_missing_cochanges(
    service: &AppService,
    dir: &str,
    changed_files: &HashSet<String>,
    min_confidence: f64,
    min_samples: usize,
    base: Option<&str>,
) -> Result<MissingCochangeReport> {
    // review では blame モードで cochange を解析する。
    // 起点ファイル = 差分に登場したファイル。
    // ただし起点が無い (差分が空) ときは何もせず空を返す。
    let source_files: Vec<String> = changed_files.iter().cloned().collect();
    if source_files.is_empty() {
        return Ok(MissingCochangeReport {
            missing: Vec::new(),
            diagnostics: Default::default(),
        });
    }
    // 起点過多 (退化した作業ツリー等で diff が全追跡ファイルに化けたケース) では
    // cochange フェーズだけを skip し、impact / API 差分 / dead 検出は継続する。
    // analyze_cochange に渡すと max_source_files ガードが InvalidRequest を返し、
    // 下の伝播フィルタが review 全体を exit 1 に落としてしまう (review には
    // 上限を制御するフラグが無く、ユーザーには回避手段が無い)。
    let max_source_files = CoChangeOptions::default().max_source_files;
    if max_source_files > 0 && source_files.len() > max_source_files {
        let mut diagnostics = crate::models::cochange::CoChangeDiagnostics {
            sources_requested: source_files.len(),
            ..Default::default()
        };
        diagnostics
            .add_reason(crate::models::cochange::CoChangeDiagnosticReason::SourceFilesExceedLimit);
        diagnostics.finalize();
        return Ok(MissingCochangeReport {
            missing: Vec::new(),
            diagnostics,
        });
    }
    // review の差分取得で使った base を blame 解析にも渡し、複数コミット範囲の
    // review でも同じ変更範囲を対象にする。base 解決失敗や git 不在は engine 側で
    // 空集合を返すので最終的に Vec::new() に落ちる。
    let opts = CoChangeOptions {
        source_files,
        base: base.map(str::to_string),
        min_confidence,
        // review だけ standalone cochange より強い support を要求する
        // (呼び出し側が 0 を渡した場合は review の既定 policy に倒す)。
        min_samples: if min_samples == 0 {
            REVIEW_COCHANGE_MIN_SAMPLES
        } else {
            min_samples
        },
        ..CoChangeOptions::default()
    };
    let cochange_result = match service.analyze_cochange(dir, &opts) {
        Ok(r) => r,
        Err(err) => {
            // 入力検証エラー (min_confidence の NaN / 範囲外等) はユーザーへ伝播する。
            // git 不在 / base 解決失敗は engine 側で empty 結果を返すため、ここまで
            // Err が来ない。InvalidRequest だけ早期失敗させて silent な誤動作を防ぐ。
            if let Some(astro_err) = err.downcast_ref::<crate::error::AstroError>()
                && astro_err.code == crate::error::ErrorCode::InvalidRequest
            {
                return Err(err);
            }
            return Ok(MissingCochangeReport {
                missing: Vec::new(),
                diagnostics: Default::default(),
            });
        }
    };

    // 各 missing file につき最も confidence が高いペアのみ残す
    let mut best: std::collections::HashMap<String, MissingCochange> =
        std::collections::HashMap::new();
    for entry in &cochange_result.entries {
        // 依存マニフェスト/ロックペアは片側変更が正規操作として頻発するためスキップ
        if is_dependency_manifest_pair(&entry.file_a, &entry.file_b) {
            continue;
        }

        let a_in_diff = changed_files.contains(&entry.file_a);
        let b_in_diff = changed_files.contains(&entry.file_b);

        let candidate = if a_in_diff && !b_in_diff {
            Some(MissingCochange {
                file: entry.file_b.clone(),
                expected_with: entry.file_a.clone(),
                confidence: entry.confidence,
                co_changes: entry.co_changes,
                denominator: entry.denominator,
            })
        } else if b_in_diff && !a_in_diff {
            Some(MissingCochange {
                file: entry.file_a.clone(),
                expected_with: entry.file_b.clone(),
                confidence: entry.confidence,
                co_changes: entry.co_changes,
                denominator: entry.denominator,
            })
        } else {
            None
        };

        if let Some(c) = candidate {
            best.entry(c.file.clone())
                .and_modify(|existing| {
                    if c.confidence > existing.confidence {
                        *existing = c.clone();
                    }
                })
                .or_insert(c);
        }
    }

    // confidence 降順でソートし最大10件に制限。
    // confidence は量子化されるため同値が並びやすく、confidence だけの stable sort だと
    // HashMap (RandomState) の反復順が同値グループ内にそのまま残る。truncate(10) が
    // 実行ごとに違う部分集合を落とすことになるので、file / expected_with で全順序にする。
    let mut missing: Vec<MissingCochange> = best.into_values().collect();
    missing.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.expected_with.cmp(&b.expected_with))
    });
    missing.truncate(10);
    Ok(MissingCochangeReport {
        missing,
        diagnostics: cochange_result.diagnostics,
    })
}
