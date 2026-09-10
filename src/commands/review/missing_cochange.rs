//! `review` の missing_cochange 警告生成。
//!
//! 「同時に変更されるべきファイルが diff に含まれていない」ことの検出であり、
//! API 差分検出 (`api_changes`) ではなく review 側の責務のためここに置く。

use anyhow::Result;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use crate::models::cochange::{CoChangeEntry, CoChangeOptions};
use crate::models::review::MissingCochange;
use crate::service::AppService;

// 依存マニフェスト/ロックの正本テーブルは `models::dependency_files` にある
// (cochange エンジンの候補除外と同じ集合を使うため)。ここでは判定関数を再エクスポートして
// 既存の呼び出し側・テストのパスを保つ。
pub(crate) use crate::models::dependency_files::is_dependency_manifest_pair;
use crate::models::dependency_files::{
    declaration_covers_source, ecosystem_for_path, is_dependency_lock_path,
};

/// 「依存宣言ファイル ↔ その言語のソースファイル」の履歴相関を warning から外すか判定する。
///
/// 依存を追加するコミットでは manifest / lock とソースが必ず一緒に変わるため、依存追加を
/// 数回繰り返したリポジトリでは両者の共変更率が 100% になる。しかしこの相関は
/// 「依存を追加したとき」限定のもので、既存関数の本体だけを書き換える変更
/// (import を 1 行も増減させない) には因果が無い。それでも履歴頻度だけを根拠に
/// 「manifest も直せ」と要求していた (実測: import 増減ゼロの差分で confidence 100%)。
///
/// standalone `cochange` は「過去に一緒に変更された」という事実を出す探索的な用途なので
/// 除外しない。review の `missing_cochanges` は「今回も変更すべき」という推奨へ変換する
/// 場所なので、因果の弱いペアはここで落とす (責務分離)。
///
/// 除外はエコシステムとプロジェクト境界の**両方**が一致する組に限る:
/// - ソースの言語が manifest の宣言対象言語に含まれること
///   (`Cargo.toml` ↔ `tools/release.py` のような別 ecosystem 間の相関は暗黙の結合かも
///   しれないので落とさない)
/// - manifest がそのソースにとって**最も近い**宣言元であること
///   (monorepo の `apps/web/package.json` ↔ `apps/api/src/main.ts` は祖先でないので別プロジェクト。
///   さらにルート `package.json` と `apps/api/package.json` が併存する場合、
///   `apps/api/src/main.ts` の宣言元は近い方だけ＝ルート manifest との相関は本物の暗黙の結合
///   かもしれないので落とさない)
///
/// 「依存を追加したのに manifest を更新し忘れた」の検出は履歴相関の仕事ではなく、
/// import と依存宣言を突き合わせる別の解析が担うべき問題。差分から「新規の外部 import が
/// あるか」を全 16 言語で判定する案は採らない — import 名と配布パッケージ名は一致せず
/// (Python)、標準ライブラリ判定は処理系バージョンに依存し、feature / plugin / build 依存は
/// import に現れず、既存 import を新たな実行経路へ載せる変更も拾えないため、
/// 「net-new import が無い」は依存が変わっていないことの証明にならない。
fn is_dependency_declaration_vs_source(dir: &str, file_a: &str, file_b: &str) -> bool {
    let paired = |declaration: &str, source: &str| {
        let Some(eco) = ecosystem_for_path(declaration) else {
            return false;
        };
        // 依存宣言ファイル同士 (manifest ↔ lock、または別 ecosystem の manifest 同士) は
        // ここでは扱わない (前段の `is_dependency_manifest_pair` の担当)。
        if ecosystem_for_path(source).is_some() {
            return false;
        }
        let Some(lang) = resolve_source_lang(dir, source) else {
            return false;
        };
        eco.langs.contains(&lang)
            && declaration_covers_source(declaration, source)
            && is_nearest_declaration(dir, eco, declaration, source)
    };
    paired(file_a, file_b) || paired(file_b, file_a)
}

/// 依存宣言ファイルが、そのソースにとって「最も近い」宣言元かを判定する。
///
/// 祖先であることだけを条件にすると、ルート `package.json` と `apps/api/package.json` が
/// 併存する monorepo で、ルート manifest を `apps/api/src/main.ts` の宣言元として扱ってしまう。
/// その結果ルート manifest との**本物の**暗黙の結合 (workspace 全体のツール設定変更など) まで
/// 消える。ソースのディレクトリから上へ辿り、最初に見つかった同一 ecosystem の manifest が
/// 与えられた declaration と同じ階層にあるときだけ真とする。
///
/// manifest が実在しない (削除済み / lock だけが残っている) 場合は false = 「除外しない」に
/// 倒す＝警告を消す方向へは倒さない。
fn is_nearest_declaration(
    dir: &str,
    eco: &crate::models::dependency_files::DependencyEcosystem,
    declaration: &str,
    source: &str,
) -> bool {
    let root = std::path::Path::new(dir);
    let decl_dir = std::path::Path::new(declaration).parent();
    let mut cur = std::path::Path::new(source).parent();
    while let Some(d) = cur {
        if root.join(d).join(eco.manifest).is_file() {
            return Some(d) == decl_dir;
        }
        if d.as_os_str().is_empty() {
            break;
        }
        cur = d.parent();
    }
    false
}

/// ファイル先頭の読み込み上限 (バイト)。shebang 判定と snapshot ヘッダ判定で共有する。
///
/// 改行までいくらでも読むと、改行を含まない巨大ファイル (minified bundle / バイナリ相当の
/// 生成物) を掴んだときにメモリを大きく食う。shebang は `#!/usr/bin/env python3` 程度、
/// snapshot ヘッダも `// Vitest Snapshot v1, https://vitest.dev/guide/snapshot.html` 程度なので
/// 256 バイトで十分。
const FILE_HEAD_PROBE_BYTES: u64 = 256;

/// ソースファイルの言語を解決する。拡張子で決まらない場合だけ先頭の shebang を見る。
///
/// `bin/tool` のような拡張子なしスクリプトは astro-sight の通常解析では shebang から
/// Python / Bash として扱われる。ここで拡張子だけを見ると source と認識できず、
/// 同じ依存追加履歴を持つ `pyproject.toml ↔ bin/tool` の誤検出が残る。
/// 読み込み失敗は `None` = 「除外しない」に倒す (従来どおり警告が出るだけで、
/// 警告を消す方向へは倒さない)。
fn resolve_source_lang(dir: &str, source: &str) -> Option<crate::language::LangId> {
    let path = camino::Utf8Path::new(source);
    if let Ok(lang) = crate::language::LangId::from_path(path) {
        return Some(lang);
    }
    // 拡張子で決まらないものだけディスクを見る。cochange の候補数は per_source_limit で
    // 絞られており、読むのは先頭 256 バイトまで。
    let full = std::path::Path::new(dir).join(source);
    let head = read_probe_head(&full)?;
    // **先頭行だけを** UTF-8 化する。256 バイト全体を `from_utf8` に通すと、shebang 自体は
    // 正しい ASCII なのに 256 バイト境界がマルチバイト文字 (日本語コメント等) の途中に来た
    // だけで判定が失敗する (実際に踏んだ)。改行位置で切ってから変換すればこれは起きない。
    //
    // 切り出した先頭行の不正 UTF-8 は valid prefix で救わず**すべて拒否**する。
    // `valid_up_to()` の prefix を使う実装は `#!/usr/bin/env python3\xff...` を Python と
    // 判定してしまい、通常の言語検出が不正 UTF-8 を拒否する挙動と食い違ったうえで
    // manifest↔source 警告を誤って抑制する。`error_len().is_none()` (末尾で列が未完) だけを
    // 許す条件も不十分で、`#!/usr/bin/env python3` + 単独の 0xE3 で EOF のような
    // 256 バイト未満のファイルまで受理してしまう。shebang 行は ASCII で 30 バイト程度なので、
    // 先頭行が 256 バイトを超えて切れることはそもそも shebang ではない = 救う必要が無い。
    let line_end = head.iter().position(|&b| b == b'\n').unwrap_or(head.len());
    let first_line = std::str::from_utf8(&head[..line_end]).ok()?;
    crate::language::LangId::from_shebang(first_line.trim_end())
}

/// 先頭バイト列を読む (shebang 判定 / snapshot ヘッダ判定で共有)。
///
/// **open と検証を一体化する**のが要点。`symlink_metadata` で確認してから `File::open` すると
/// その間に通常ファイルを symlink / FIFO へ差し替えられ、open がパスを再解決してしまう
/// (TOCTOU)。Unix では `O_NOFOLLOW | O_NONBLOCK`、Windows では
/// `FILE_FLAG_OPEN_REPARSE_POINT` で「リンクを辿らずに開く」ことを指定したうえで、
/// **開いた descriptor 自身**の metadata で regular file を確認する。
///
/// 失敗はすべて `None` = 「除外しない」に倒す。
fn read_probe_head(path: &std::path::Path) -> Option<Vec<u8>> {
    use std::io::Read;

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // O_NOFOLLOW: symlink 自体を開こうとしてエラーにする (差し替えを検出)
        // O_NONBLOCK: FIFO / デバイスを開く際に無期限ブロックしない
        //
        // 拒否できるのは**最終コンポーネント**の symlink だけ。祖先ディレクトリの symlink まで
        // 弾くには component-wise な `openat` が必要だが、ここで扱うパスは git 由来かつ
        // `dir` 配下に制限された相対パスなので、その範囲は要件に含めない。
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_OPEN_REPARSE_POINT (0x0020_0000): reparse point (symlink / junction) を
        // 辿らずそれ自体を開く。付けないと通常の open がリンク先を解決してしまい、
        // 後段の descriptor metadata 検査もリンク先を見るため symlink を受理してしまう。
        // 開けた場合も metadata の file_type が regular file にならないので下で弾かれる。
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path).ok()?;
    // descriptor 自身の型を見るのでパス再解決による差し替えを受けない。
    if !file.metadata().ok()?.file_type().is_file() {
        return None;
    }
    let mut head = Vec::new();
    file.take(FILE_HEAD_PROBE_BYTES)
        .read_to_end(&mut head)
        .ok()?;
    Some(head)
}

/// テストから `resolve_source_lang` を直接叩くためのアクセサ。
///
/// symlink / マルチバイト境界の扱いは cochange の履歴条件に依存せず固定したいので、
/// 判定関数そのものを検証する経路を用意する。
#[cfg(test)]
pub(crate) fn resolve_source_lang_for_test(
    dir: &str,
    source: &str,
) -> Option<crate::language::LangId> {
    resolve_source_lang(dir, source)
}

/// 外部 snapshot ファイルの先頭行として認める既知のヘッダ。
///
/// **部分一致では判定しない**。`Snapshot` の語を含むだけの手書き fixture や、snapshot 以外の
/// 用途で `.snap` を使うファイルまで巻き込むと、本物の暗黙の結合を消してしまう。既知のランナーが
/// 実際に書き出す行と完全一致した場合だけ「生成出力」と認める。
///
/// 未知のランナー・将来のバージョン・URL 変更はここに載らないので抑制されない
/// ＝従来どおり候補として出る (安全側)。
const SNAPSHOT_FILE_HEADERS: &[&str] = &[
    // Jest。v1 の案内 URL は短縮 URL 時代と現行ドキュメントの 2 種がある。
    "// Jest Snapshot v1, https://goo.gl/fbAQLP",
    "// Jest Snapshot v1, https://jestjs.io/docs/snapshot-testing",
    // Vitest
    "// Vitest Snapshot v1, https://vitest.dev/guide/snapshot.html",
    // Bun。Jest 互換 API で `__snapshots__` 規約も同じ。
    "// Bun Snapshot v1, https://bun.sh/docs/test/snapshots",
];

/// 外部 snapshot のパスから、それを生成したテストファイルのパスを導出する。
///
/// Jest / Vitest / Bun が共有する標準規約は「テストファイルと同じディレクトリの
/// `__snapshots__/` へ、**テストファイル名そのまま** + `.snap` で書き出す」。
///
/// ```text
/// tests/__snapshots__/widget.test.tsx.snap  →  tests/widget.test.tsx
/// ```
///
/// snapshot 名が元の拡張子まで含むため `widget.test.ts` と `widget.test.tsx` が併存しても
/// 曖昧にならない。拡張子を取り替えて探索したり別ディレクトリを走査したりはしない
/// (規約から一意に決まるものだけを扱う)。
///
/// 規約に合わないパスは `None` = 「方向付けしない」に倒す。
fn snapshot_source_test_path(snapshot: &str) -> Option<String> {
    let path = camino::Utf8Path::new(snapshot);
    // `.snap` は 1 回だけ剥がす。`x.snap.snap` の内側までは辿らない。
    let stem = path.file_name()?.strip_suffix(".snap")?;
    if stem.is_empty() {
        return None;
    }
    let snapshots_dir = path.parent()?;
    if snapshots_dir.file_name()? != "__snapshots__" {
        return None;
    }
    let test_dir = snapshots_dir.parent()?;
    // cochange のパスは git 由来の `/` 区切り正規形なので、`Utf8Path::join` ではなく `/` で連結する
    // (`join` は Windows で `\` を挿入するため、導出パスが欠落候補と一致せず抑制が効かなくなる)。
    // `__snapshots__` がワークスペース直下なら親は空パスになり、stem そのものになる。
    Some(if test_dir.as_str().is_empty() {
        stem.to_string()
    } else {
        format!("{}/{stem}", test_dir.as_str())
    })
}

/// 先頭バイト列の 1 行目が既知の snapshot ヘッダと完全一致するか。
///
/// 改行が見つからない場合は認めない — 読み込み上限で行が途切れた可能性があり
/// 「先頭行がヘッダと一致した」と言えないため (判定不能は抑制しない側に倒す)。
fn head_is_snapshot_file(head: &[u8]) -> bool {
    let Some(line_end) = head.iter().position(|&b| b == b'\n') else {
        return false;
    };
    let mut line = &head[..line_end];
    // CRLF で書き出された snapshot も同じ 1 行として扱う。
    if line.last() == Some(&b'\r') {
        line = &line[..line.len() - 1];
    }
    let Ok(text) = std::str::from_utf8(line) else {
        return false;
    };
    SNAPSHOT_FILE_HEADERS.contains(&text)
}

/// 「欠落側 = 生成元テスト / diff にある側 = その snapshot」の関係が標準規約で一意に確定するか。
/// true なら missing_cochange の推薦から外す。
///
/// snapshot は被テスト対象の出力が変わったときにも更新されるので、テストと snapshot の履歴相関は
/// **双方向ではない**。テストを変えたら snapshot も更新するのが普通だが、snapshot を更新したから
/// といってテストを変える必要はない。履歴頻度だけを根拠に逆方向の変更漏れを要求すると、
/// 期待値を更新するたびに同じ誤検出が出る。
///
/// これは「テスト変更が不要だと証明した」わけではない (期待値だけ更新して必要なテストロジックの
/// 変更を忘れることはある)。**標準の生成関係にあるペアについて、履歴相関だけでは逆方向の変更要求を
/// 出さない**という推薦方針にすぎない。
///
/// `.gitattributes` の `linguist-generated` は見ない。あちらは「生成物一般」の宣言で、除外すると
/// 両方向とも候補から消える。ここは実ファイルのヘッダで生成出力を確認したうえで**方向だけ**を
/// 付けるので、判定経路が独立している。
///
/// **判定不能はすべて `false` を返し、既存の missing 候補を残す。** 読み込み失敗・非通常ファイル・
/// 未知のヘッダ・規約に合わないパスがこれに当たる。
fn is_snapshot_generated_from(dir: &str, source_test: &str, snapshot: &str) -> bool {
    let Some(derived) = snapshot_source_test_path(snapshot) else {
        return false;
    };
    if derived != source_test {
        return false;
    }
    // 生成元テストが実在する通常ファイルであること。過去リビジョンから読み戻してまでは
    // 確認しない — 削除済みのテストに対して抑制すると、消し忘れた snapshot の検出まで消える。
    if read_probe_head(&std::path::Path::new(dir).join(source_test)).is_none() {
        return false;
    }
    // パス規約だけでは手書き fixture や別用途の `.snap` を巻き込むため、生成出力であることを
    // ファイル自身のヘッダで確認する。
    let Some(head) = read_probe_head(&std::path::Path::new(dir).join(snapshot)) else {
        return false;
    };
    head_is_snapshot_file(&head)
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

/// `MissingCochange` の公開形式を変えず、cochange エンジンが計算したランキング情報を
/// review の重複排除・上位 10 件選択まで保持する。
#[derive(Debug, Clone)]
struct RankedMissingCochange {
    item: MissingCochange,
    ranking: f64,
    is_history_evidence: bool,
}

impl RankedMissingCochange {
    fn new(item: MissingCochange, entry: &CoChangeEntry, smoothing_on: bool) -> Self {
        Self {
            item,
            ranking: entry.ranking_value(smoothing_on),
            is_history_evidence: entry.is_history_evidence(),
        }
    }
}

/// cochange エンジンと同じ基準で候補を比較する。
///
/// raw confidence だけで比較すると、3/3 の小標本が 30/40 の十分な標本より上位になり、
/// エンジン側で計算した Bayesian smoothing の順位を review が壊してしまう。
fn compare_ranked_missing(a: &RankedMissingCochange, b: &RankedMissingCochange) -> Ordering {
    b.ranking
        .partial_cmp(&a.ranking)
        .unwrap_or(Ordering::Equal)
        .then_with(|| b.item.co_changes.cmp(&a.item.co_changes))
        .then_with(|| {
            b.item
                .confidence
                .partial_cmp(&a.item.confidence)
                .unwrap_or(Ordering::Equal)
        })
        .then_with(|| a.is_history_evidence.cmp(&b.is_history_evidence))
        .then_with(|| a.item.file.cmp(&b.item.file))
        .then_with(|| a.item.expected_with.cmp(&b.item.expected_with))
}

fn insert_best_missing(
    best: &mut HashMap<String, RankedMissingCochange>,
    candidate: RankedMissingCochange,
) {
    best.entry(candidate.item.file.clone())
        .and_modify(|existing| {
            if compare_ranked_missing(&candidate, existing).is_lt() {
                *existing = candidate.clone();
            }
        })
        .or_insert(candidate);
}

pub(crate) fn detect_missing_cochanges(
    service: &AppService,
    dir: &str,
    changed_files: &HashSet<String>,
    min_confidence: f64,
    min_samples: usize,
    base: Option<&str>,
    // 生成物 (`.gitattributes` の `linguist-generated` / ヘッダマーカー) を起点・候補に
    // 残すか。CLI のグローバル `--include-generated` (config.toml の `skip_generated`) を
    // そのまま流す＝standalone の cochange と review で同じ指定が効く。
    include_generated: bool,
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
        include_generated,
        ..CoChangeOptions::default()
    };
    let smoothing_on = !opts.disable_smoothing;
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

    // 各 missing file につき、エンジンと同じランキングで最良のペアのみ残す。
    let mut best: HashMap<String, RankedMissingCochange> = HashMap::new();
    for entry in &cochange_result.entries {
        // 依存マニフェスト/ロックペアは片側変更が正規操作として頻発するためスキップ
        if is_dependency_manifest_pair(&entry.file_a, &entry.file_b) {
            continue;
        }
        // 依存宣言ファイル ↔ ソースの履歴相関は「依存を追加したとき」限定の条件付き相関で、
        // 本体だけの変更には因果が無いため review の推奨からは外す。
        if is_dependency_declaration_vs_source(dir, &entry.file_a, &entry.file_b) {
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

        if let Some(candidate) = candidate {
            // snapshot → 生成元テストの方向だけ推薦から外す。
            //
            // **重複排除より前に判定する**のが要点。`insert_best_missing` は欠落ファイルごとに
            // 最良の 1 ペアだけを残すので、選択後に落とすと同じテストについて別の変更ファイルから
            // 得られた正当な候補まで失われる。
            //
            // 逆方向 (テストを変更したのに snapshot が欠けている) では `expected_with` が snapshot の
            // 規約を満たさないため述語は false を返し、従来どおり候補に残る。
            //
            // `--include-generated` (config の `skip_generated = false`) は「生成物を特別扱いしない」
            // という利用者の意図なので、この方向付けも無効化する＝解除手段を cochange 側と揃える。
            if !include_generated
                && is_snapshot_generated_from(dir, &candidate.file, &candidate.expected_with)
            {
                continue;
            }
            insert_best_missing(
                &mut best,
                RankedMissingCochange::new(candidate, entry, smoothing_on),
            );
        }
    }

    // エンジンの順位を保ったまま最大 10 件へ絞る。path まで含む全順序なので、
    // HashMap (RandomState) の反復順に左右されず出力は決定的になる。
    let mut ranked: Vec<RankedMissingCochange> = best.into_values().collect();
    ranked.sort_by(compare_ranked_missing);
    ranked.truncate(10);
    let missing = ranked.into_iter().map(|candidate| candidate.item).collect();
    Ok(MissingCochangeReport {
        missing,
        diagnostics: cochange_result.diagnostics,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(
        file: &str,
        expected_with: &str,
        confidence: f64,
        co_changes: usize,
        ranking: f64,
    ) -> RankedMissingCochange {
        RankedMissingCochange {
            item: MissingCochange {
                file: file.to_string(),
                expected_with: expected_with.to_string(),
                confidence,
                co_changes,
                denominator: Some(co_changes),
                evidence: None,
            },
            ranking,
            is_history_evidence: false,
        }
    }

    #[test]
    fn smoothed_ranking_is_preserved_for_deduplication_and_sorting() {
        let small_sample = candidate("missing.rs", "small.rs", 1.0, 3, 0.33);
        let sufficient_sample = candidate("missing.rs", "stable.rs", 0.75, 30, 0.64);
        let another = candidate("another.rs", "source.rs", 0.9, 9, 0.5);

        let mut best = HashMap::new();
        insert_best_missing(&mut best, small_sample);
        insert_best_missing(&mut best, sufficient_sample);
        insert_best_missing(&mut best, another);

        let mut ranked: Vec<_> = best.into_values().collect();
        ranked.sort_by(compare_ranked_missing);

        assert_eq!(ranked[0].item.expected_with, "stable.rs");
        assert_eq!(ranked[1].item.file, "another.rs");
    }

    /// snapshot パスから生成元テストを導出できる条件を、規約に合う形と合わない形の
    /// **対照**で固定する。取りこぼし (None) は「方向付けしない = 従来どおり候補に出る」
    /// なので安全側だが、規約外のパスから誤って導出すると本物の共変更を消す。
    #[test]
    fn snapshot_source_test_path_resolves_only_the_standard_convention() {
        // 規約どおり: テストファイル名そのまま + `.snap` が同階層の `__snapshots__` にある
        assert_eq!(
            snapshot_source_test_path("tests/__snapshots__/widget.test.tsx.snap").as_deref(),
            Some("tests/widget.test.tsx")
        );
        // snapshot 名が元の拡張子まで含むので `.ts` と `.tsx` は衝突しない
        assert_eq!(
            snapshot_source_test_path("tests/__snapshots__/widget.test.ts.snap").as_deref(),
            Some("tests/widget.test.ts")
        );
        // ワークスペース直下の `__snapshots__` でも親が空パスになるだけ
        assert_eq!(
            snapshot_source_test_path("__snapshots__/a-test.jsx.snap").as_deref(),
            Some("a-test.jsx")
        );
        // 深い階層でもディレクトリはそのまま保つ (別ディレクトリの同名は導出しない)
        assert_eq!(
            snapshot_source_test_path("src/ui/__snapshots__/card.test.tsx.snap").as_deref(),
            Some("src/ui/card.test.tsx")
        );

        // 対照: 規約から外れるものは導出しない
        assert_eq!(
            snapshot_source_test_path("tests/widget.test.tsx.snap"),
            None,
            "`__snapshots__` 配下でない `.snap` は対象外"
        );
        assert_eq!(
            snapshot_source_test_path("tests/__snapshots__/widget.test.tsx"),
            None,
            "`.snap` で終わらないファイルは対象外"
        );
        assert_eq!(
            snapshot_source_test_path("tests/snapshots/widget.test.tsx.snap"),
            None,
            "ディレクトリ名が正確に `__snapshots__` でなければ対象外"
        );
        assert_eq!(
            snapshot_source_test_path("tests/__snapshots__/nested/widget.test.tsx.snap"),
            None,
            "直上ディレクトリが `__snapshots__` でなければ対象外"
        );
        assert_eq!(
            snapshot_source_test_path("tests/__snapshots__/.snap"),
            None,
            "剥がした結果が空になるパスは対象外"
        );
        // `.snap` は 1 回だけ剥がす (繰り返し除去への退行を防ぐ)。導出先 `tests/x.snap` が
        // 実在しなければ `is_snapshot_generated_from` 側で抑制対象から外れる。
        assert_eq!(
            snapshot_source_test_path("tests/__snapshots__/x.snap.snap").as_deref(),
            Some("tests/x.snap")
        );
    }

    /// ヘッダ照合が**完全一致**であることを、既知・未知・部分一致の対照で固定する。
    ///
    /// 部分一致で通すと、`Snapshot` の語を含むだけの手書き fixture を生成物と誤認して
    /// 本物の共変更を消してしまう。
    #[test]
    fn head_is_snapshot_file_requires_an_exact_known_header() {
        for header in SNAPSHOT_FILE_HEADERS {
            let head = format!("{header}\n\nexports[`x 1`] = `y`;\n");
            assert!(
                head_is_snapshot_file(head.as_bytes()),
                "既知ヘッダは認定する: {header}"
            );
            // CRLF で書き出された snapshot も同じ 1 行として扱う
            let crlf = format!("{header}\r\n\r\nexports[`x 1`] = `y`;\r\n");
            assert!(
                head_is_snapshot_file(crlf.as_bytes()),
                "CRLF でも認定する: {header}"
            );
        }

        // 対照: 認定してはいけないもの
        assert!(
            !head_is_snapshot_file(b"// Jest Snapshot v2, https://goo.gl/fbAQLP\n"),
            "未知のバージョンは認定しない (従来どおり候補に出る)"
        );
        assert!(
            !head_is_snapshot_file(b"// Some Snapshot of the old layout\n"),
            "`Snapshot` を含むだけの手書きコメントは認定しない"
        );
        assert!(
            !head_is_snapshot_file(b"# fixture\n// Jest Snapshot v1, https://goo.gl/fbAQLP\n"),
            "2 行目以降にヘッダがあっても認定しない"
        );
        assert!(
            !head_is_snapshot_file(b"// Jest Snapshot v1, https://goo.gl/fbAQLP"),
            "改行が無い = 読み込み上限で途切れた可能性があるので認定しない"
        );
        assert!(
            !head_is_snapshot_file(b"// Jest Snapshot v1, https://goo.gl/fbAQLP extra\n"),
            "前方一致では認定しない"
        );
        assert!(!head_is_snapshot_file(b""), "空ファイルは認定しない");
        assert!(
            !head_is_snapshot_file(&[0xff, 0xfe, b'\n']),
            "不正 UTF-8 は認定しない"
        );
    }

    /// `is_snapshot_generated_from` の積 (パス規約 × 生成元の実在 × ヘッダ) を、
    /// 1 つずつ崩した対照で固定する。判定不能はすべて false = 候補を残す。
    #[test]
    fn is_snapshot_generated_from_requires_path_existing_source_and_header() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path();
        let dir_str = dir.to_str().expect("utf-8 path");
        let header = "// Vitest Snapshot v1, https://vitest.dev/guide/snapshot.html";

        std::fs::create_dir_all(dir.join("tests/__snapshots__")).expect("mkdir");
        std::fs::write(dir.join("tests/widget.test.tsx"), "test('x', () => {});\n").expect("write");
        std::fs::write(
            dir.join("tests/__snapshots__/widget.test.tsx.snap"),
            format!("{header}\n\nexports[`x 1`] = `<div />`;\n"),
        )
        .expect("write");

        assert!(
            is_snapshot_generated_from(
                dir_str,
                "tests/widget.test.tsx",
                "tests/__snapshots__/widget.test.tsx.snap"
            ),
            "パス規約・生成元の実在・ヘッダが揃えば抑制対象"
        );

        // 対照 1: 方向が逆 (欠落側が snapshot) なら抑制しない
        assert!(
            !is_snapshot_generated_from(
                dir_str,
                "tests/__snapshots__/widget.test.tsx.snap",
                "tests/widget.test.tsx"
            ),
            "テスト → snapshot の方向は維持する"
        );

        // 対照 2: 別ディレクトリの同名テストへは対応付けない
        std::fs::create_dir_all(dir.join("other")).expect("mkdir");
        std::fs::write(dir.join("other/widget.test.tsx"), "test('x', () => {});\n").expect("write");
        assert!(
            !is_snapshot_generated_from(
                dir_str,
                "other/widget.test.tsx",
                "tests/__snapshots__/widget.test.tsx.snap"
            ),
            "導出先と欠落候補のディレクトリが違えば抑制しない"
        );

        // 対照 3: ヘッダの無い `.snap` (手書き fixture) は抑制しない
        std::fs::create_dir_all(dir.join("fixtures/__snapshots__")).expect("mkdir");
        std::fs::write(dir.join("fixtures/data.test.ts"), "test('y', () => {});\n").expect("write");
        std::fs::write(
            dir.join("fixtures/__snapshots__/data.test.ts.snap"),
            "hand written fixture\n",
        )
        .expect("write");
        assert!(
            !is_snapshot_generated_from(
                dir_str,
                "fixtures/data.test.ts",
                "fixtures/__snapshots__/data.test.ts.snap"
            ),
            "同じパス規約でもヘッダが無ければ抑制しない"
        );

        // 対照 4: 生成元テストが実在しなければ抑制しない
        std::fs::write(
            dir.join("tests/__snapshots__/removed.test.tsx.snap"),
            format!("{header}\n\nexports[`x 1`] = `<div />`;\n"),
        )
        .expect("write");
        assert!(
            !is_snapshot_generated_from(
                dir_str,
                "tests/removed.test.tsx",
                "tests/__snapshots__/removed.test.tsx.snap"
            ),
            "生成元テストが実在しなければ抑制しない (消し忘れ snapshot の検出を消さない)"
        );
    }
}
