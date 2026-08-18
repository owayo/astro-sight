//! `review` の missing_cochange 警告生成。
//!
//! 「同時に変更されるべきファイルが diff に含まれていない」ことの検出であり、
//! API 差分検出 (`api_changes`) ではなく review 側の責務のためここに置く。

use anyhow::Result;
use std::collections::HashSet;

use crate::models::cochange::CoChangeOptions;
use crate::models::review::MissingCochange;
use crate::service::AppService;

// 依存マニフェスト/ロックの正本テーブルは `models::dependency_files` にある
// (cochange エンジンの候補除外と同じ集合を使うため)。ここでは判定関数を再エクスポートして
// 既存の呼び出し側・テストのパスを保つ。
pub(crate) use crate::models::dependency_files::is_dependency_manifest_pair;
use crate::models::dependency_files::{is_dependency_lock_path, is_dependency_manifest_path};

/// 「依存宣言ファイル ↔ その言語のソースファイル」の履歴相関を warning から外すか判定する。
///
/// 依存を追加するコミットでは manifest / lock とソースが必ず一緒に変わるため、依存追加を
/// 数回繰り返したリポジトリでは両者の共変更率が 100% になる。しかしこの相関は
/// 「依存を追加したとき」という条件付きのもので、既存関数の本体だけを書き換える変更
/// (import を 1 行も増減させない) には因果が無い。それでも履歴頻度だけを根拠に
/// 「manifest も直せ」と要求していた (実測: import 増減ゼロの差分で confidence 100%)。
///
/// standalone `cochange` は「過去に一緒に変更された」という事実を出す探索的な用途なので
/// 除外しない。review の `missing_cochanges` は「今回も変更すべき」という推奨へ変換する
/// 場所なので、因果の弱いペアはここで落とす (責務分離)。
///
/// 「依存を追加したのに manifest を更新し忘れた」の検出は履歴相関の仕事ではなく、
/// import と依存宣言を突き合わせる別の解析が担うべき問題。差分から「新規の外部 import が
/// あるか」を全 16 言語で判定する案は採らない — import 名と配布パッケージ名は一致せず
/// (Python)、標準ライブラリ判定は処理系バージョンに依存し、feature / plugin / build 依存は
/// import に現れず、既存 import を新たな実行経路へ載せる変更も拾えないため、
/// 「net-new import が無い」は依存が変わっていないことの証明にならない。
fn is_dependency_declaration_vs_source(file_a: &str, file_b: &str) -> bool {
    let is_dep = |p: &str| is_dependency_manifest_path(p) || is_dependency_lock_path(p);
    let is_source = |p: &str| {
        crate::language::LangId::from_path(camino::Utf8Path::new(p)).is_ok()
            // manifest 自体が解析対象言語 (pyproject.toml など) に解決されることはないが、
            // 将来の言語追加で重なっても依存宣言側の判定を優先する。
            && !is_dep(p)
    };
    (is_dep(file_a) && is_source(file_b)) || (is_dep(file_b) && is_source(file_a))
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
    //
    // ロックファイルは起点にしない。生成物なので「lock を変えたなら X も変えろ」という
    // 推奨に意味が無く (依存更新コマンドの副産物)、依存追加コミットで一緒に変わった
    // ソースを軒並み相方として引き当てるだけになる。engine 側の候補除外
    // (`CoChangeExclude`) は相方側にしか効かないため、起点側はここで落とす。
    let source_files: Vec<String> = changed_files
        .iter()
        .filter(|f| !is_dependency_lock_path(f))
        .cloned()
        .collect();
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
        // 依存宣言ファイル ↔ ソースの履歴相関は「依存を追加したとき」限定の条件付き相関で、
        // 本体だけの変更には因果が無いため review の推奨からは外す。
        if is_dependency_declaration_vs_source(&entry.file_a, &entry.file_b) {
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
                evidence: entry.evidence,
            })
        } else if b_in_diff && !a_in_diff {
            Some(MissingCochange {
                file: entry.file_a.clone(),
                expected_with: entry.file_b.clone(),
                confidence: entry.confidence,
                co_changes: entry.co_changes,
                denominator: entry.denominator,
                evidence: entry.evidence,
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
