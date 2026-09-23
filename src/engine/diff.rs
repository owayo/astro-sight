use std::collections::HashSet;

use crate::models::impact::{DiffFile, HunkInfo};

/// `--- a/<path>` / `+++ b/<path>` 形式のファイルヘッダ行から `<path>` を取り出す。
/// `line` が `prefix` で始まらなければ `None`。
///
/// git はラベル (接頭辞 + パス) に空白を含むとき、行末に TAB を付けて出力する
/// (`+++ b/my lib.rs\t`)。GNU diff も TAB の後ろにタイムスタンプを置く。クォートされない
/// パスは TAB を含み得ない (TAB を含むパスは git が `"a/x\ty"` の C 形式でクォートし、
/// そもそもこの接頭辞に一致しない) ため、最初の TAB 以降を落とせば元のパスに戻る。
/// 落とさないと `"src/my lib.rs\t"` が存在確認で外れ、そのファイルの変更全体が
/// 解析対象から黙って消える。ヘッダを解析する箇所はすべてこの関数を通すこと。
pub(crate) fn strip_header_path<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(prefix)?;
    Some(rest.split_once('\t').map_or(rest, |(path, _)| path))
}

/// hunk 本体の 1 行を分類した結果。
pub(crate) enum HunkBodyLine<'a> {
    /// 追加行 (`+`)。先頭の `+` を除いた内容を保持する。
    Added(&'a str),
    /// 削除行 (`-`)。先頭の `-` を除いた内容を保持する。
    Removed(&'a str),
    /// コンテキスト行 (` ` または空行)。
    Context,
    /// `\ No newline at end of file` 等の metadata 行 (行数に数えない)。
    Metadata,
}

/// hunk ヘッダで宣言された old/new の残り行数を追跡し、本体行を消費する。
///
/// 本体を消費し切る (`is_complete`) まではファイル/hunk ヘッダ判定を行わないことで、
/// 削除/追加行のコンテンツが `--- a/...` / `+++ b/...` の形でも誤認しない
/// (hunk 途中で次ファイルヘッダと誤判定し以降の本体が脱落する false negative の防止)。
pub(crate) struct HunkProgress {
    old_remaining: usize,
    new_remaining: usize,
}

impl HunkProgress {
    pub(crate) fn new(hunk: &HunkInfo) -> Self {
        Self {
            old_remaining: hunk.old_count,
            new_remaining: hunk.new_count,
        }
    }

    /// 本体 1 行を消費して old/new の残数を減らし、行種別を返す。
    pub(crate) fn consume<'a>(&mut self, line: &'a str) -> HunkBodyLine<'a> {
        match line.as_bytes().first() {
            Some(b'+') => {
                self.new_remaining = self.new_remaining.saturating_sub(1);
                HunkBodyLine::Added(&line[1..])
            }
            Some(b'-') => {
                self.old_remaining = self.old_remaining.saturating_sub(1);
                HunkBodyLine::Removed(&line[1..])
            }
            // `\ No newline at end of file` は old/new いずれの行数にも数えない。
            Some(b'\\') => HunkBodyLine::Metadata,
            _ => {
                // 先頭スペースの context 行、および末尾の空行を context として扱う。
                self.old_remaining = self.old_remaining.saturating_sub(1);
                self.new_remaining = self.new_remaining.saturating_sub(1);
                HunkBodyLine::Context
            }
        }
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.old_remaining == 0 && self.new_remaining == 0
    }
}

/// diff 全体を 1 回だけ走査し、`+++ b/<path>` ヘッダで始まる区間を new 側パスごとに
/// 引けるようにした索引。
///
/// `extract_changed_line_facts` / `detect_signature_changes` / `is_symbol_in_changed_lines` /
/// `has_deletion_in_new_range` はいずれも「対象ファイル以外の区間は読み飛ばす」だけなので、
/// diff 全体の代わりに対象ファイルの区間だけを渡しても結果は変わらない。影響分析の Pass 1 は
/// これらをファイルごと (シンボルごと) に呼ぶため、diff 全体を渡すと diff 長 × ファイル数の
/// 2 乗になっていた (実測: context が 1000 / 2000 / 3000 ファイルで 2.1 / 7.6 / 17.1 秒)。
///
/// 区間の境界は各関数の状態機械と同じ規約で決める: hunk 本体を消費中の行は (ヘッダに
/// 見えても) 境界にしない。hunk 外の `--- ` / `+++ ` 行で区間を閉じ、`+++ b/<path>` なら
/// 新しい区間を開く。区間は常に「hunk 外・直前の区間が閉じた状態」から始まるので、
/// 対象ファイルの区間だけを順に連結した入力は、diff 全体を渡したときと同じ状態遷移を辿る。
pub(crate) struct FileSections<'a> {
    input: &'a str,
    /// new 側パス → その区間のバイト範囲 (出現順)。同じパスが複数回現れる入力でも
    /// 全区間を順に連結すれば diff 全体を渡した場合と同じ結果になる。
    ranges_by_path: std::collections::HashMap<&'a str, Vec<std::ops::Range<usize>>>,
}

impl<'a> FileSections<'a> {
    pub(crate) fn split(input: &'a str) -> Self {
        let mut ranges_by_path: std::collections::HashMap<&'a str, Vec<std::ops::Range<usize>>> =
            std::collections::HashMap::new();
        let mut current: Option<(&'a str, usize)> = None;
        let mut active_hunk: Option<HunkProgress> = None;
        let mut offset = 0usize;
        for raw in input.split_inclusive('\n') {
            let line_start = offset;
            offset += raw.len();
            // `str::lines` と同じ行の切り出し (`\n` を落とし、続く `\r` も落とす)。
            let line = raw
                .strip_suffix('\n')
                .map_or(raw, |l| l.strip_suffix('\r').unwrap_or(l));
            if let Some(progress) = active_hunk.as_mut() {
                progress.consume(line);
                if progress.is_complete() {
                    active_hunk = None;
                }
                continue;
            }
            if line.starts_with("--- ") || line.starts_with("+++ ") {
                if let Some((path, start)) = current.take() {
                    ranges_by_path
                        .entry(path)
                        .or_default()
                        .push(start..line_start);
                }
                if let Some(path) = strip_header_path(line, "+++ b/") {
                    current = Some((path, line_start));
                }
            } else if line.starts_with("@@ ")
                && let Some(hunk) = parse_hunk_header(line)
            {
                active_hunk = Some(HunkProgress::new(&hunk));
            }
        }
        if let Some((path, start)) = current {
            ranges_by_path
                .entry(path)
                .or_default()
                .push(start..input.len());
        }
        Self {
            input,
            ranges_by_path,
        }
    }

    /// `path` の区間を返す (該当なしは空文字列)。区間が 1 つなら元の diff を借用する。
    pub(crate) fn get(&self, path: &str) -> std::borrow::Cow<'a, str> {
        match self.ranges_by_path.get(path).map(Vec::as_slice) {
            None | Some([]) => std::borrow::Cow::Borrowed(""),
            Some([range]) => std::borrow::Cow::Borrowed(&self.input[range.clone()]),
            Some(ranges) => std::borrow::Cow::Owned(
                ranges
                    .iter()
                    .map(|range| &self.input[range.clone()])
                    .collect(),
            ),
        }
    }
}

/// 単一ファイルの unified diff から、new 側で実際に追加された行 (`+` 行) の
/// 0-indexed 行番号 set を抽出する。
///
/// `find_affected_symbols` は HunkInfo の `new_start..new_start+new_count` 全域
/// (= context 行込み) で symbol range と overlap 判定するため、隣接 hunk の
/// context 3 行に巻き込まれた未変更 symbol が affected に残る (Issue
/// 2026-05-14-private-const-and-unchanged-export-noise)。
/// 本関数で返す set を symbol range と照合することで、`+` 行が 1 つも range に
/// 入らない symbol を context-only overlap として弾ける。
///
/// 注意:
/// - **削除は本 set に現れない** (`-` 行は new 側に行を持たない)。削除位置も必要なら
///   [`extract_changed_line_facts`] を使い `deletion_gaps` と併用すること。追加行だけで
///   「変更あり」を判定すると、context 付き diff の削除のみ hunk (`new_count > 0` になる)
///   と重なったシンボルを取りこぼす。
/// - file_path は `+++ b/<path>` の `<path>` と完全一致で照合する。
pub fn extract_changed_new_lines(input: &str, file_path: &str) -> HashSet<usize> {
    extract_changed_line_facts(input, file_path).added_lines
}

/// 単一ファイルの unified diff から抽出した「new 側の変更事実」。
///
/// 追加は new 側に**実在する行**として表せるが、削除は new 側に行を持たず
/// 「行と行の**あいだ**」にしか現れない。両者を 1 つの set に混ぜると表現できないため
/// 別フィールドに分ける。
#[derive(Debug, Default, Clone)]
pub struct ChangedLineFacts {
    /// `+` 行の 0-indexed 行番号。
    pub added_lines: HashSet<usize>,
    /// 削除位置。値は**その削除の直後に来る new 側行の 0-indexed 行番号**
    /// (ファイル末尾での削除なら最終行 + 1)。
    pub deletion_gaps: HashSet<usize>,
}

/// 単一ファイルの unified diff から、new 側の追加行と削除位置 (gap) を抽出する。
///
/// `extract_changed_new_lines` は追加行しか返さないため、削除だけの hunk では
/// 「symbol range 内に変更あり」を表現できない。astro-sight 自身の diff 生成
/// (`git_input.rs`) は `-U` 未指定 = context 3 行なので、行削除には必ず context が付いて
/// `new_count > 0` になる。その結果 `find_affected_symbols` の context-only フィルタが
/// 「range 内に `+` 行が無い」を理由に、削除と重なった全シンボルを捨てていた
/// (利用中の object literal メンバー削除などが review / api / dead-code すべてで無音になる)。
pub fn extract_changed_line_facts(input: &str, file_path: &str) -> ChangedLineFacts {
    let mut facts = ChangedLineFacts::default();
    let mut in_target_file = false;
    let mut active_hunk: Option<HunkProgress> = None;
    let mut current_new_line: usize = 0;

    for line in input.lines() {
        // hunk 本体を消費中はヘッダに見える行も本体行として扱う。target 外の hunk でも
        // count を追跡し、本体内の `+++ b/<target>` で対象ファイルへ誤って切り替わるのを防ぐ。
        if let Some(progress) = active_hunk.as_mut() {
            let consumed = progress.consume(line);
            if in_target_file {
                match consumed {
                    HunkBodyLine::Added(_) => {
                        if current_new_line > 0 {
                            facts.added_lines.insert(current_new_line - 1);
                        }
                        current_new_line += 1;
                    }
                    HunkBodyLine::Context => {
                        current_new_line += 1;
                    }
                    // 削除行は new 側に行を持たないので行番号を進めない。代わりに
                    // 「次に現れる new 側行」を gap として記録する。
                    HunkBodyLine::Removed(_) => {
                        if current_new_line > 0 {
                            facts.deletion_gaps.insert(current_new_line - 1);
                        }
                    }
                    // metadata 行 (`\ No newline at end of file` 等) は行数に数えない。
                    HunkBodyLine::Metadata => {}
                }
            }
            if progress.is_complete() {
                active_hunk = None;
            }
            continue;
        }

        if line.starts_with("--- ") {
            in_target_file = false;
        } else if let Some(path) = strip_header_path(line, "+++ b/") {
            in_target_file = path == file_path;
        } else if line.starts_with("+++ ") {
            in_target_file = false;
        } else if line.starts_with("@@ ")
            && let Some(hunk) = parse_hunk_header(line)
        {
            // ゼロ幅 hunk (`-U0` の純削除、例 `@@ -5 +4,0 @@`) の `new_start` は
            // 「削除が起きた位置の**直前**にある new 側行」を 1-indexed で指す。gap の規約は
            // 「削除の**直後**に来る new 側行」なので、ここで 1 行進めて座標系を揃える
            // (ゼロ幅 hunk は定義上 `+` 行を持たないため added_lines への影響はない)。
            current_new_line = if hunk.new_count == 0 {
                hunk.new_start.saturating_add(1)
            } else {
                hunk.new_start
            };
            active_hunk = Some(HunkProgress::new(&hunk));
        }
    }

    facts
}

/// 対象ファイル 1 件分の diff を new 側ソースへ逆適用して復元した変更前 (old 側) のソース。
pub(crate) struct ReconstructedOldSource {
    /// 復元した old 側ソース (各行を `\n` で終端する)。
    pub(crate) source: Vec<u8>,
    /// 削除された (`-` 行の) old 側 0-indexed 行番号。
    pub(crate) removed_lines: HashSet<usize>,
}

/// `input` のうち `file_path` の hunk を `new_source` に逆適用し、old 側ソースを復元する。
///
/// unified diff は new 側と old 側の対応を完全に持つので、new 側ファイル + diff から
/// old 側を再構成できる (git を呼ばないので `--diff` / stdin 入力でも使える)。
/// hunk の context / 追加行が `new_source` の内容と一致しない、hunk が逆順・重複する、
/// 行番号が範囲外になるなど、diff とファイルが食い違う場合は `None` を返す
/// (食い違った入力から組み立てた old 側で判定しない)。
pub(crate) fn reconstruct_old_source(
    input: &str,
    file_path: &str,
    new_source: &[u8],
) -> Option<ReconstructedOldSource> {
    // `str::lines` と同じく、末尾の改行の後ろに空行を作らず、行末の `\r` は比較から外す。
    let mut new_lines: Vec<&[u8]> = new_source.split(|&b| b == b'\n').collect();
    if new_source.ends_with(b"\n") {
        new_lines.pop();
    }
    let trim_cr =
        |line: &[u8]| -> usize { line.strip_suffix(b"\r").map_or(line.len(), <[u8]>::len) };
    let same_line = |source_line: &[u8], diff_text: &str| {
        source_line[..trim_cr(source_line)] == *diff_text.as_bytes()
    };

    let mut out: Vec<u8> = Vec::with_capacity(new_source.len());
    let mut removed_lines = HashSet::new();
    let mut old_line = 0usize; // 出力済みの old 側行数
    let mut new_idx = 0usize; // 次に消費する new 側行 (0-indexed)
    let mut in_target_file = false;
    let mut active_hunk: Option<HunkProgress> = None;

    for line in input.lines() {
        if let Some(progress) = active_hunk.as_mut() {
            let consumed = progress.consume(line);
            if in_target_file {
                match consumed {
                    HunkBodyLine::Context => {
                        // context 行は先頭の空白 1 文字を落とす (空行として出力された context も許す)。
                        let text = line.strip_prefix(' ').unwrap_or(line);
                        let source_line = new_lines.get(new_idx)?;
                        if !same_line(source_line, text) {
                            return None;
                        }
                        out.extend_from_slice(source_line);
                        out.push(b'\n');
                        old_line += 1;
                        new_idx += 1;
                    }
                    HunkBodyLine::Added(text) => {
                        if !same_line(new_lines.get(new_idx)?, text) {
                            return None;
                        }
                        new_idx += 1;
                    }
                    HunkBodyLine::Removed(text) => {
                        removed_lines.insert(old_line);
                        out.extend_from_slice(text.as_bytes());
                        out.push(b'\n');
                        old_line += 1;
                    }
                    HunkBodyLine::Metadata => {}
                }
            }
            if progress.is_complete() {
                active_hunk = None;
            }
            continue;
        }

        if line.starts_with("--- ") {
            in_target_file = false;
        } else if let Some(path) = strip_header_path(line, "+++ b/") {
            in_target_file = path == file_path;
        } else if line.starts_with("+++ ") {
            in_target_file = false;
        } else if line.starts_with("@@ ")
            && let Some(hunk) = parse_hunk_header(line)
        {
            if in_target_file {
                // ゼロ幅側の start は「挿入 / 削除位置の直前の行」(1-indexed) を指す。
                let hunk_new_start = if hunk.new_count == 0 {
                    hunk.new_start
                } else {
                    hunk.new_start - 1
                };
                let hunk_old_start = if hunk.old_count == 0 {
                    hunk.old_start
                } else {
                    hunk.old_start - 1
                };
                if hunk_new_start < new_idx {
                    return None;
                }
                // hunk の手前までは old / new で同一の行。
                while new_idx < hunk_new_start {
                    out.extend_from_slice(new_lines.get(new_idx)?);
                    out.push(b'\n');
                    old_line += 1;
                    new_idx += 1;
                }
                if old_line != hunk_old_start {
                    return None;
                }
            }
            active_hunk = Some(HunkProgress::new(&hunk));
        }
    }
    // 対象ファイルの hunk が宣言した行数に届かないまま入力が終わった (途中で切れた diff)。
    // 残りを「変更なし」とみなして組み立てると、切れた後ろの `-` 行 (宣言ヘッダなど) を
    // 失った old 側で判定することになる。
    if in_target_file && active_hunk.is_some() {
        return None;
    }
    for source_line in new_lines.get(new_idx..)? {
        out.extend_from_slice(source_line);
        out.push(b'\n');
    }
    Some(ReconstructedOldSource {
        source: out,
        removed_lines,
    })
}

/// 指定ファイルの unified diff に、new 側の行範囲 `[start_line, end_line]` (0-indexed)
/// 内で発生した削除 (`-` 行) があるかを判定する。
///
/// 削除行自体は new 側に存在しないため、「削除が起きた位置」は直後に続く new 側行の
/// 0-indexed 行番号で近似する。**厳密な行対応ではなく近似判定専用**で、範囲境界ぎわの削除は
/// 保守側 (true = 削除あり) に倒れる。フォーマッタの再整形やコメント行の削除でも true になるが、
/// 呼び出し側 (impact のオブジェクトリテラル変数「メンバー追加のみ」判定 = フィルタ 3c) では
/// true = 「フィルタを適用せず従来どおり cross-file 検索する」なので検出漏れ側には倒れない。
/// この非対称性 (false 側だけが検索を省く) に依存しない用途へ転用しないこと。
pub(crate) fn has_deletion_in_new_range(
    input: &str,
    file_path: &str,
    start_line: usize,
    end_line: usize,
) -> bool {
    let mut in_target_file = false;
    let mut active_hunk: Option<HunkProgress> = None;
    // 1-indexed: 次に出力される new 側行番号
    let mut current_new_line: usize = 0;

    for line in input.lines() {
        // hunk 本体を消費中はヘッダに見える行も本体行として扱う (extract_changed_new_lines と同じ規約)。
        if let Some(progress) = active_hunk.as_mut() {
            let consumed = progress.consume(line);
            if in_target_file {
                match consumed {
                    HunkBodyLine::Added(_) | HunkBodyLine::Context => {
                        current_new_line += 1;
                    }
                    HunkBodyLine::Removed(_) => {
                        let pos = current_new_line.saturating_sub(1);
                        if pos >= start_line && pos <= end_line {
                            return true;
                        }
                    }
                    HunkBodyLine::Metadata => {}
                }
            }
            if progress.is_complete() {
                active_hunk = None;
            }
            continue;
        }
        if line.starts_with("--- ") {
            in_target_file = false;
        } else if let Some(path) = strip_header_path(line, "+++ b/") {
            in_target_file = path == file_path;
        } else if line.starts_with("+++ ") {
            in_target_file = false;
        } else if line.starts_with("@@ ")
            && let Some(hunk) = parse_hunk_header(line)
        {
            current_new_line = hunk.new_start;
            active_hunk = Some(HunkProgress::new(&hunk));
        }
    }
    false
}

/// unified diff 文字列を `DiffFile` の配列に変換する。
///
/// 削除ファイル (`+++ /dev/null`) の hunk 内 `-` 行は旧ソース復元用に蓄積し、
/// `DiffFile.deleted_old_source` にセットする。`extract_exported_symbols_from_git`
/// が base mismatch で失敗した際の API 差分検出フォールバックで使う。
pub fn parse_unified_diff(input: &str) -> Vec<DiffFile> {
    let mut files = Vec::new();
    let mut current_old_path: Option<String> = None;
    let mut current_new_path: Option<String> = None;
    let mut current_hunks: Vec<HunkInfo> = Vec::new();
    let mut current_deleted_lines: Vec<u8> = Vec::new();
    let mut active_hunk: Option<HunkProgress> = None;

    for line in input.lines() {
        // hunk 本体を消費中は、`--- a/...` / `+++ b/...` に見える行も本体行として扱い、
        // ファイルヘッダ判定へ落とさない (削除/追加行のコンテンツが diff ヘッダと衝突する
        // ケースで以降の hunk 本体が脱落する false negative を防ぐ)。
        if let Some(progress) = active_hunk.as_mut() {
            let consumed = progress.consume(line);
            if current_new_path.as_deref() == Some("/dev/null")
                && let HunkBodyLine::Removed(removed) = consumed
            {
                // 削除ファイルの hunk 内 `-` 行を旧ソース順で蓄積する。
                current_deleted_lines.extend_from_slice(removed.as_bytes());
                current_deleted_lines.push(b'\n');
            }
            if progress.is_complete() {
                active_hunk = None;
            }
            continue;
        }

        if let Some(path) = strip_header_path(line, "--- a/") {
            // 直前のファイル情報を確定
            flush_file(
                &mut files,
                &mut current_old_path,
                &mut current_new_path,
                &mut current_hunks,
                &mut current_deleted_lines,
            );
            current_old_path = Some(path.to_string());
        } else if line.starts_with("--- /dev/null") {
            flush_file(
                &mut files,
                &mut current_old_path,
                &mut current_new_path,
                &mut current_hunks,
                &mut current_deleted_lines,
            );
            current_old_path = Some("/dev/null".to_string());
        } else if line.starts_with("--- ") {
            // 認識できない旧側ヘッダ (quotepath クォート `--- "a/..."` 等)。
            // 直前ファイルの state を引きずると以降の hunk が誤帰属するため、
            // flush して当該ファイルを解析対象から外す (fail-safe)。
            flush_file(
                &mut files,
                &mut current_old_path,
                &mut current_new_path,
                &mut current_hunks,
                &mut current_deleted_lines,
            );
        } else if let Some(path) = strip_header_path(line, "+++ b/") {
            current_new_path = Some(path.to_string());
        } else if line.starts_with("+++ /dev/null") {
            current_new_path = Some("/dev/null".to_string());
        } else if line.starts_with("+++ ") {
            // 認識できない新側ヘッダ。片側だけ認識できたペア (`--- a/x` + `+++ "b/..."`) を
            // 別ファイルの hunk と合成しないよう、新側を未確定に戻す。
            current_new_path = None;
        } else if line.starts_with("@@ ")
            && let Some(hunk) = parse_hunk_header(line)
        {
            active_hunk = Some(HunkProgress::new(&hunk));
            current_hunks.push(hunk);
        }
    }

    // 最後のファイル情報を確定
    flush_file(
        &mut files,
        &mut current_old_path,
        &mut current_new_path,
        &mut current_hunks,
        &mut current_deleted_lines,
    );

    files
}

fn flush_file(
    files: &mut Vec<DiffFile>,
    old_path: &mut Option<String>,
    new_path: &mut Option<String>,
    hunks: &mut Vec<HunkInfo>,
    deleted_lines: &mut Vec<u8>,
) {
    if let (Some(old), Some(new)) = (old_path.take(), new_path.take())
        && !hunks.is_empty()
    {
        let deleted_old_source = if new == "/dev/null" && !deleted_lines.is_empty() {
            Some(std::mem::take(deleted_lines))
        } else {
            None
        };
        files.push(DiffFile {
            old_path: old,
            new_path: new,
            hunks: std::mem::take(hunks),
            deleted_old_source,
        });
    }
    hunks.clear();
    deleted_lines.clear();
}

/// 内容が変わっていない rename (`git mv` 後の `rename from` / `rename to` だけのブロック) を
/// `hunks` が空の `DiffFile` として返す。
///
/// `parse_unified_diff` は hunk を持つファイルだけを返すため、内容同一の rename は現れない
/// (dead-code / cochange はこの前提で揃えている)。一方 API 差分では、対応言語でないファイルへの
/// rename (`api.ts` → `api.ts.bak`) が「旧ファイルの API がすべて消える」変更になるため、
/// 必要な呼び出し側だけが個別に取り込めるよう別関数にしている。
/// C 形式でクォートされたパス (`rename from "a\tb"`) は安全に復元できないので返さない。
pub fn parse_hunkless_renames(input: &str) -> Vec<DiffFile> {
    #[derive(Default)]
    struct Block {
        from: Option<String>,
        to: Option<String>,
        has_content: bool,
    }
    impl Block {
        fn finish(self, out: &mut Vec<DiffFile>) {
            if let (Some(old_path), Some(new_path)) = (self.from, self.to)
                && !self.has_content
            {
                out.push(DiffFile {
                    old_path,
                    new_path,
                    hunks: Vec::new(),
                    deleted_old_source: None,
                });
            }
        }
    }
    let unquoted = |path: &str| (!path.starts_with('"')).then(|| path.to_string());

    let mut out = Vec::new();
    let mut block = Block::default();
    for line in input.lines() {
        if line.starts_with("diff --git ") {
            std::mem::take(&mut block).finish(&mut out);
        } else if let Some(path) = line.strip_prefix("rename from ") {
            block.from = unquoted(path);
        } else if let Some(path) = line.strip_prefix("rename to ") {
            block.to = unquoted(path);
        } else if line.starts_with("--- ")
            || line.starts_with("+++ ")
            || line.starts_with("@@ ")
            || line.starts_with("Binary files ")
        {
            // 内容の差分があるブロックは `parse_unified_diff` の担当。hunk 本体の行は
            // 必ず ` ` / `+` / `-` で始まるので、ここより前に `rename` 行と誤認することはない。
            block.has_content = true;
        }
    }
    block.finish(&mut out);
    out
}

/// `"@@ -10,5 +10,8 @@"` や `"@@ -10,5 +10,8 @@ fn foo()"` の hunk ヘッダを解析する。
pub(crate) fn parse_hunk_header(line: &str) -> Option<HunkInfo> {
    // 先頭の `"@@ "` を除去
    let rest = line.strip_prefix("@@ ")?;
    // 終端の `" @@"` の位置を探す
    let end = rest.find(" @@")?;
    let range_part = &rest[..end];

    // old/new の範囲に分割: "-10,5 +10,8"
    let mut parts = range_part.split_whitespace();
    let old_part = parts.next()?.strip_prefix('-')?;
    let new_part = parts.next()?.strip_prefix('+')?;

    let (old_start, old_count) = parse_range_spec(old_part)?;
    let (new_start, new_count) = parse_range_spec(new_part)?;

    // unified diff 仕様: count > 0 のとき start は 1-origin で必ず 1 以上。
    // start = 0 が正当なのは count = 0 (片側完全空) の hunk のみ。
    // count > 0 && start = 0 の不正ヘッダ (例: `@@ -1,0 +0,1 @@`) は reject し、
    // 後段の current_new_line 補正 (`current_new_line > 0` ガード) が黙って
    // 1 行目の `+` 行を取りこぼす false negative を防ぐ。
    if (old_count > 0 && old_start == 0) || (new_count > 0 && new_start == 0) {
        return None;
    }

    Some(HunkInfo {
        old_start,
        old_count,
        new_start,
        new_count,
    })
}

/// `"10,5"` または `"10"` を `(start, count)` に変換する。
fn parse_range_spec(spec: &str) -> Option<(usize, usize)> {
    if let Some((start, count)) = spec.split_once(',') {
        Some((start.parse().ok()?, count.parse().ok()?))
    } else {
        Some((spec.parse().ok()?, 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// git はラベル (接頭辞 + パス) に空白を含むとき `--- a/my lib.rs\t` のように行末へ TAB を
    /// 付ける。TAB ごとパスに取り込むと `"src/my lib.rs\t"` が存在確認で外れ、そのファイルの
    /// 変更全体が黙って解析対象から消えていた。GNU diff の `\t<timestamp>` も同じ規約で落とす。
    #[test]
    fn strip_header_path_drops_git_tab_suffix_and_timestamps() {
        assert_eq!(
            strip_header_path("+++ b/src/my lib.rs\t", "+++ b/"),
            Some("src/my lib.rs")
        );
        assert_eq!(
            strip_header_path(
                "--- a/src/x.rs\t2026-01-01 00:00:00.000000000 +0900",
                "--- a/"
            ),
            Some("src/x.rs")
        );
        // 対照: TAB の無いヘッダはそのまま、接頭辞が違えば None。
        assert_eq!(
            strip_header_path("+++ b/src/x.rs", "+++ b/"),
            Some("src/x.rs")
        );
        assert_eq!(strip_header_path("+++ /dev/null", "+++ b/"), None);
    }

    /// 空白を含むパスの実 git 出力 (ヘッダ行末に TAB) を、ヘッダを読む全関数が同じパスとして
    /// 扱うこと。1 箇所でも TAB を取り込むと、そのファイルだけ変更行や hunk が消える。
    #[test]
    fn header_paths_with_git_tab_suffix_are_recognized_everywhere() {
        let path = "src/my util.ts";
        let diff = concat!(
            "diff --git a/src/my util.ts b/src/my util.ts\n",
            "index 0f62e86..33c1eb4 100644\n",
            "--- a/src/my util.ts\t\n",
            "+++ b/src/my util.ts\t\n",
            "@@ -1,3 +1,3 @@\n",
            "-export function helper(a: number): number {\n",
            "-  return a + 1;\n",
            "+export function helper(a: number, b: number): number {\n",
            "+  return a + b;\n",
            " }\n",
        );
        let files = parse_unified_diff(diff);
        assert_eq!(files.len(), 1, "{files:?}");
        assert_eq!(files[0].old_path, path);
        assert_eq!(files[0].new_path, path);

        let facts = extract_changed_line_facts(diff, path);
        assert_eq!(facts.added_lines, HashSet::from([0, 1]));
        assert!(has_deletion_in_new_range(diff, path, 0, 2));
        let header_start = diff.find("+++").expect("new-side header");
        assert_eq!(FileSections::split(diff).get(path), &diff[header_start..]);
        let reconstructed = reconstruct_old_source(
            diff,
            path,
            b"export function helper(a: number, b: number): number {\n  return a + b;\n}\n",
        )
        .expect("reconstruct");
        assert_eq!(
            String::from_utf8(reconstructed.source).expect("utf-8"),
            "export function helper(a: number): number {\n  return a + 1;\n}\n"
        );
    }

    /// `FileSections` で切り出した区間を渡しても、diff 全体を渡した場合と結果が一致すること
    /// (影響分析 Pass 1 の 2 乗解消が出力を変えないことの固定)。
    ///
    /// 同じファイルが 2 回現れる入力 / 削除ファイル / hunk 本体がヘッダに見える行 /
    /// `\ No newline at end of file` / CRLF / ヘッダの TAB を 1 つの diff に混ぜる。
    #[test]
    fn file_sections_give_same_results_as_full_diff() {
        let diff = concat!(
            "diff --git a/a.rs b/a.rs\n",
            "--- a/a.rs\n",
            "+++ b/a.rs\n",
            "@@ -1,3 +1,3 @@\n",
            " fn keep() {}\n",
            "-fn a(x: u32) {}\n",
            "+fn a(x: u32, y: u32) {}\n",
            " fn tail() {}\n",
            "diff --git a/gone.rs b/gone.rs\n",
            "deleted file mode 100644\n",
            "--- a/gone.rs\n",
            "+++ /dev/null\n",
            "@@ -1,2 +0,0 @@\n",
            "-fn gone() {}\n",
            "-fn a(x: u64) {}\n",
            "diff --git a/b c.rs b/b c.rs\n",
            "--- a/b c.rs\t\n",
            "+++ b/b c.rs\t\n",
            "@@ -1,2 +1,3 @@\n",
            // hunk 本体の削除行 / 追加行がファイルヘッダに見える (本体として読む)。
            "--- a/a.rs\n",
            "+++ b/a.rs\n",
            "+fn b(z: u8) {}\n",
            " fn c() {}\r\n",
            "\\ No newline at end of file\n",
            "diff --git a/a.rs b/a.rs\n",
            "--- a/a.rs\n",
            "+++ b/a.rs\n",
            "@@ -10,2 +10,1 @@\n",
            "-fn removed_late() {}\n",
            " fn late() {}\n",
        );
        let sections = FileSections::split(diff);
        for path in ["a.rs", "b c.rs", "gone.rs", "missing.rs"] {
            let section = sections.get(path);
            assert!(
                section.len() < diff.len(),
                "{path}: 区間は diff 全体より短いこと (空振り防止)"
            );
            let full = extract_changed_line_facts(diff, path);
            let part = extract_changed_line_facts(&section, path);
            assert_eq!(full.added_lines, part.added_lines, "{path}: added_lines");
            assert_eq!(
                full.deletion_gaps, part.deletion_gaps,
                "{path}: deletion_gaps"
            );
            for (start, end) in [(0, 0), (0, 5), (8, 12)] {
                assert_eq!(
                    has_deletion_in_new_range(diff, path, start, end),
                    has_deletion_in_new_range(&section, path, start, end),
                    "{path}: has_deletion_in_new_range({start}, {end})"
                );
            }
        }
        // 対照: 期待どおりの中身が区間に入っていること (同じファイルの 2 区間を両方拾う)。
        let a = extract_changed_line_facts(&sections.get("a.rs"), "a.rs");
        assert_eq!(a.added_lines, HashSet::from([1]));
        assert_eq!(a.deletion_gaps, HashSet::from([1, 9]));
        let bc = extract_changed_line_facts(&sections.get("b c.rs"), "b c.rs");
        assert_eq!(bc.added_lines, HashSet::from([0, 1]));
        assert!(sections.get("missing.rs").is_empty());
    }

    /// diff を new 側へ逆適用して old 側を復元する。移動 (削除 hunk + 追加 hunk) も
    /// 1 ファイル内で正しく並べ戻し、削除行の old 側行番号を返す。
    #[test]
    fn reconstruct_old_source_reverses_moves_and_reports_removed_lines() {
        let old = "fn helper(a: u32) -> u32 {\n    a\n}\n\nfn other() {}\nfn third() {}\n";
        let new =
            "fn other() {}\nfn third() {}\n\nfn helper(a: u32, b: u32) -> u32 {\n    a + b\n}\n";
        let diff = concat!(
            "--- a/src/util.rs\n",
            "+++ b/src/util.rs\n",
            "@@ -1,5 +1,1 @@\n",
            "-fn helper(a: u32) -> u32 {\n",
            "-    a\n",
            "-}\n",
            "-\n",
            " fn other() {}\n",
            "@@ -6 +2,5 @@\n",
            " fn third() {}\n",
            "+\n",
            "+fn helper(a: u32, b: u32) -> u32 {\n",
            "+    a + b\n",
            "+}\n",
        );
        let rec = reconstruct_old_source(diff, "src/util.rs", new.as_bytes()).expect("reconstruct");
        assert_eq!(String::from_utf8(rec.source).expect("utf-8"), old);
        assert_eq!(rec.removed_lines, HashSet::from([0, 1, 2, 3]));

        // `-U0` の純追加 / 純削除 hunk (ゼロ幅側の start は直前の行を指す)。
        let u0 = concat!(
            "--- a/f.rs\n",
            "+++ b/f.rs\n",
            "@@ -1,0 +2 @@\n",
            "+inserted\n",
            "@@ -3 +3,0 @@\n",
            "-dropped\n",
        );
        let rec = reconstruct_old_source(u0, "f.rs", b"a\ninserted\nb\n").expect("reconstruct -U0");
        assert_eq!(
            String::from_utf8(rec.source).expect("utf-8"),
            "a\nb\ndropped\n"
        );
        assert_eq!(rec.removed_lines, HashSet::from([2]));

        // 対照: diff とファイル内容が食い違えば復元しない (食い違った old 側で判定しない)。
        assert!(reconstruct_old_source(diff, "src/util.rs", b"unrelated\n").is_none());
        // 対象ファイルの hunk が無ければ new 側がそのまま old 側。
        let untouched = reconstruct_old_source(diff, "other.rs", b"x\n").expect("untouched");
        assert_eq!(untouched.source, b"x\n");
        assert!(untouched.removed_lines.is_empty());
    }

    /// 対象ファイルの hunk が宣言した行数に届かないまま diff が終わったら復元しない。
    ///
    /// 旧実装は残りを「変更なし」とみなして Some を返していたため、切れた後ろにある
    /// `-` 行 (ここでは旧シグネチャ) を失った old 側で宣言ヘッダを比較することになっていた。
    #[test]
    fn reconstruct_old_source_rejects_truncated_hunk_of_target_file() {
        let new = "fn helper(a: u32, b: u32) -> u32 {\n    a + b\n}\n";
        let full = concat!(
            "--- a/src/util.rs\n",
            "+++ b/src/util.rs\n",
            "@@ -1,3 +1,3 @@\n",
            "+fn helper(a: u32, b: u32) -> u32 {\n",
            "+    a + b\n",
            "-fn helper(a: u32) -> u32 {\n",
            "-    a\n",
            " }\n",
        );
        // 対照: 完全な diff なら旧シグネチャを復元できる。
        let rec = reconstruct_old_source(full, "src/util.rs", new.as_bytes()).expect("complete");
        assert_eq!(
            String::from_utf8(rec.source).expect("utf-8"),
            "fn helper(a: u32) -> u32 {\n    a\n}\n"
        );
        // hunk 本体の途中 (`-` 行の手前) で切れた diff。
        let truncated = full.split_inclusive('\n').take(5).collect::<String>();
        assert!(reconstruct_old_source(&truncated, "src/util.rs", new.as_bytes()).is_none());
        // 切れているのが対象外のファイルなら、対象ファイルの復元には影響しない。
        let other_truncated =
            format!("{full}--- a/other.rs\n+++ b/other.rs\n@@ -1,2 +1,2 @@\n-x\n");
        assert!(reconstruct_old_source(&other_truncated, "src/util.rs", new.as_bytes()).is_some());
    }

    #[test]
    fn parse_simple_diff() {
        let diff = r#"diff --git a/src/main.rs b/src/main.rs
index abc1234..def5678 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -10,5 +10,8 @@ fn main() {
+    new_line();
     existing();
-    removed();
"#;
        let files = parse_unified_diff(diff);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].new_path, "src/main.rs");
        assert_eq!(files[0].hunks.len(), 1);
        assert_eq!(files[0].hunks[0].old_start, 10);
        assert_eq!(files[0].hunks[0].old_count, 5);
        assert_eq!(files[0].hunks[0].new_start, 10);
        assert_eq!(files[0].hunks[0].new_count, 8);
    }

    #[test]
    fn parse_multi_file_diff() {
        // 各 hunk の count を本体行数と一致させた簡略 diff (追加 1 行ずつ)。
        let diff = r#"--- a/src/foo.rs
+++ b/src/foo.rs
@@ -1,0 +1,1 @@
+use bar;
--- a/src/bar.rs
+++ b/src/bar.rs
@@ -5,0 +5,1 @@
+fn new_fn() {}
@@ -20,0 +21,1 @@
+// comment
"#;
        let files = parse_unified_diff(diff);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].new_path, "src/foo.rs");
        assert_eq!(files[0].hunks.len(), 1);
        assert_eq!(files[1].new_path, "src/bar.rs");
        assert_eq!(files[1].hunks.len(), 2);
    }

    #[test]
    fn parse_hunk_no_count() {
        let hunk = parse_hunk_header("@@ -1 +1 @@");
        assert!(hunk.is_some());
        let h = hunk.unwrap();
        assert_eq!(h.old_start, 1);
        assert_eq!(h.old_count, 1);
        assert_eq!(h.new_start, 1);
        assert_eq!(h.new_count, 1);
    }

    #[test]
    fn parse_new_file_diff_with_dev_null() {
        let diff = r#"diff --git a/src/new.rs b/src/new.rs
new file mode 100644
index 0000000..1234567
--- /dev/null
+++ b/src/new.rs
@@ -0,0 +1,2 @@
+fn new_fn() {}
+"#;
        let files = parse_unified_diff(diff);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].old_path, "/dev/null");
        assert_eq!(files[0].new_path, "src/new.rs");
        // 新規ファイルでは旧ソース無し
        assert!(files[0].deleted_old_source.is_none());
    }

    #[test]
    fn parse_deleted_file_captures_old_source() {
        // 削除ファイルの hunk 内 `-` 行を旧ソースとして保持できることを確認する
        let diff = r#"diff --git a/src/old.rs b/src/old.rs
deleted file mode 100644
index 1234567..0000000
--- a/src/old.rs
+++ /dev/null
@@ -1,3 +0,0 @@
-pub fn removed_fn() {
-    println!("gone");
-}
"#;
        let files = parse_unified_diff(diff);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].old_path, "src/old.rs");
        assert_eq!(files[0].new_path, "/dev/null");
        let restored = files[0]
            .deleted_old_source
            .as_ref()
            .expect("deleted file should have old source");
        let text = std::str::from_utf8(restored).expect("utf-8");
        assert_eq!(text, "pub fn removed_fn() {\n    println!(\"gone\");\n}\n");
    }

    #[test]
    fn parse_deleted_file_skips_no_newline_marker() {
        // `\ No newline at end of file` などの diff metadata 行は旧ソースに混入させない
        let diff = "diff --git a/foo.txt b/foo.txt\ndeleted file mode 100644\n--- a/foo.txt\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-only line\n\\ No newline at end of file\n";
        let files = parse_unified_diff(diff);
        assert_eq!(files.len(), 1);
        let restored = files[0].deleted_old_source.as_ref().expect("captured");
        assert_eq!(std::str::from_utf8(restored).unwrap(), "only line\n");
    }

    /// `+` 行のみを new_line ベースで集計する。context 行と `-` 行は new_line を
    /// 進めるが set には含めない。
    #[test]
    fn extract_changed_new_lines_records_added_lines_only() {
        let diff = "--- a/foo.rs\n+++ b/foo.rs\n@@ -10,3 +10,4 @@\n existing\n+added_at_line_11\n existing2\n existing3\n";
        let changed = extract_changed_new_lines(diff, "foo.rs");
        // new_start=10, line 10 ' existing' (context), line 11 '+added' (changed),
        // line 12 ' existing2' (context), line 13 ' existing3' (context)
        // 0-indexed: 11 - 1 = 10
        let mut sorted: Vec<_> = changed.into_iter().collect();
        sorted.sort();
        assert_eq!(sorted, vec![10]);
    }

    /// 削除行は new_line を進めず、`+` 直前の add 行だけ記録される。
    #[test]
    fn extract_changed_new_lines_handles_deletion_correctly() {
        let diff =
            "--- a/foo.rs\n+++ b/foo.rs\n@@ -1,3 +1,3 @@\n line_a\n-deleted\n+added\n line_c\n";
        let changed = extract_changed_new_lines(diff, "foo.rs");
        // new_start=1, line 1 ' line_a' (context), '-deleted' (skip new_line),
        // line 2 '+added' (changed → 0-indexed 1), line 3 ' line_c' (context)
        let mut sorted: Vec<_> = changed.into_iter().collect();
        sorted.sort();
        assert_eq!(sorted, vec![1]);
    }

    /// `deletion_gaps` の座標規約を固定する。
    ///
    /// gap は「その削除の**直後**に来る new 側行の 0-indexed 行番号」。連続削除は同じ位置を
    /// 指すので 1 つに畳まれ (存在確認にしか使わないので情報は失われない)、ファイル末尾での
    /// 削除は「最終行 + 1」になる。
    ///
    /// ゼロ幅 hunk (`-U0` の純削除) の `new_start` は削除位置の**直前**行を 1-indexed で
    /// 指すため、context 付き hunk と座標系が 1 行ずれる。両形式で同じ編集が同じ gap を返す
    /// ことを対で固定する (この補正が無いと `ChangedLineFacts` の契約に反する)。
    #[test]
    fn extract_changed_line_facts_records_deletion_gaps() {
        // 中間削除 (context 付き): a b [c を削除] d → 残る new 側は 0-indexed 0,1,2
        // 削除の直後に来る new 側行は 0-indexed 2 (`d`)。
        let mid = "--- a/foo.rs\n+++ b/foo.rs\n@@ -1,4 +1,3 @@\n a\n b\n-c\n d\n";
        let facts = extract_changed_line_facts(mid, "foo.rs");
        assert!(facts.added_lines.is_empty(), "削除のみなので `+` 行は無い");
        assert_eq!(facts.deletion_gaps, HashSet::from([2]));

        // 同じ編集を `-U0` で表現したもの。new_start=2 は削除直前の new 側行 (1-indexed)。
        let mid_u0 = "--- a/foo.rs\n+++ b/foo.rs\n@@ -3 +2,0 @@\n-c\n";
        let facts_u0 = extract_changed_line_facts(mid_u0, "foo.rs");
        assert_eq!(
            facts_u0.deletion_gaps, facts.deletion_gaps,
            "context 付きと `-U0` で同じ gap を返すこと"
        );

        // 連続する末尾削除 (context 付き): a b [c d を削除] → 残る new 側は 0-indexed 0,1
        // gap は 1 つに畳まれ、最終行 (1) + 1 = 2 になる。
        let tail = "--- a/foo.rs\n+++ b/foo.rs\n@@ -1,4 +1,2 @@\n a\n b\n-c\n-d\n";
        let tail_facts = extract_changed_line_facts(tail, "foo.rs");
        assert_eq!(
            tail_facts.deletion_gaps,
            HashSet::from([2]),
            "連続削除は 1 gap に畳まれ、末尾削除は最終行 + 1 を指す"
        );

        // 同じ末尾削除を `-U0` で。
        let tail_u0 = "--- a/foo.rs\n+++ b/foo.rs\n@@ -3,2 +2,0 @@\n-c\n-d\n";
        let tail_u0_facts = extract_changed_line_facts(tail_u0, "foo.rs");
        assert_eq!(
            tail_u0_facts.deletion_gaps, tail_facts.deletion_gaps,
            "末尾の連続削除も context 付きと `-U0` で一致すること"
        );

        // 対照: 削除の無い純追加では gap を作らない。
        let add_only = "--- a/foo.rs\n+++ b/foo.rs\n@@ -1,2 +1,3 @@\n a\n+b\n c\n";
        let add_facts = extract_changed_line_facts(add_only, "foo.rs");
        assert!(
            add_facts.deletion_gaps.is_empty(),
            "追加のみの hunk は gap を作らない: {add_facts:?}"
        );
        assert_eq!(add_facts.added_lines, HashSet::from([1]));
    }

    /// 対象ファイルでない `+` 行は無視する。
    #[test]
    fn extract_changed_new_lines_skips_other_files() {
        let diff = "--- a/foo.rs\n+++ b/foo.rs\n@@ -1 +1,2 @@\n existing\n+foo_added\n--- a/bar.rs\n+++ b/bar.rs\n@@ -1 +1,2 @@\n existing\n+bar_added\n";
        let changed = extract_changed_new_lines(diff, "foo.rs");
        assert_eq!(changed.len(), 1, "foo.rs の add のみ");
        assert!(changed.contains(&1)); // line 2 → 0-indexed 1
        let bar_changed = extract_changed_new_lines(diff, "bar.rs");
        assert_eq!(bar_changed.len(), 1);
        assert!(bar_changed.contains(&1));
    }

    /// pure-add (new file) では全 `+` 行が記録される。
    #[test]
    fn extract_changed_new_lines_pure_add_collects_all_lines() {
        let diff = "--- /dev/null\n+++ b/new.rs\n@@ -0,0 +1,3 @@\n+line1\n+line2\n+line3\n";
        let changed = extract_changed_new_lines(diff, "new.rs");
        let mut sorted: Vec<_> = changed.into_iter().collect();
        sorted.sort();
        assert_eq!(sorted, vec![0, 1, 2]);
    }

    /// 追加のみ (削除行なし) の hunk では has_deletion_in_new_range は false。
    #[test]
    fn has_deletion_in_new_range_pure_add_returns_false() {
        // 0-indexed 6..=9 のオブジェクトリテラルに 1 行追加 (new 側 8 行目 = 0-indexed 7)
        let diff = "--- a/mod.ts\n+++ b/mod.ts\n@@ -6,4 +6,5 @@\n export const api = {\n   alpha,\n+  gamma,\n   beta,\n };\n";
        assert!(!has_deletion_in_new_range(diff, "mod.ts", 5, 10));
    }

    /// 範囲内で行が削除 (書き換え含む) されていれば true。
    #[test]
    fn has_deletion_in_new_range_detects_removal_inside_range() {
        let diff = "--- a/mod.ts\n+++ b/mod.ts\n@@ -6,4 +6,4 @@\n export const api = {\n   alpha,\n-  beta,\n+  beta: betaV2,\n };\n";
        assert!(has_deletion_in_new_range(diff, "mod.ts", 5, 9));
    }

    /// 範囲外の削除は false (対象シンボルの range に閉じて判定する)。
    #[test]
    fn has_deletion_in_new_range_ignores_removal_outside_range() {
        let diff = "--- a/mod.ts\n+++ b/mod.ts\n@@ -1,3 +1,2 @@\n line1\n-removed\n line3\n";
        // 削除位置は 0-indexed 1 付近 → 範囲 [5,10] の外
        assert!(!has_deletion_in_new_range(diff, "mod.ts", 5, 10));
        assert!(has_deletion_in_new_range(diff, "mod.ts", 0, 2));
    }

    /// 他ファイルの削除行は対象ファイルの判定に影響しない。
    #[test]
    fn has_deletion_in_new_range_skips_other_files() {
        let diff = "--- a/other.ts\n+++ b/other.ts\n@@ -1,2 +1,1 @@\n keep\n-removed\n--- a/mod.ts\n+++ b/mod.ts\n@@ -1,1 +1,2 @@\n keep\n+added\n";
        assert!(!has_deletion_in_new_range(diff, "mod.ts", 0, 5));
        assert!(has_deletion_in_new_range(diff, "other.ts", 0, 5));
    }

    /// hunk 本体の削除/追加行コンテンツが `--- a/...` / `+++ b/...` の形でも、
    /// ファイルヘッダと誤認せず後続の本体を取りこぼさない (false negative 回帰防止)。
    #[test]
    fn parse_unified_diff_body_line_looks_like_header() {
        // hunk は old=2,new=2。削除行コンテンツが `-- a/x` (diff 行で `--- a/x`)、
        // 追加行コンテンツが `++ b/x` (diff 行で `+++ b/x`)。
        let diff = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,2 +1,2 @@\n--- a/x\n+++ b/x\n";
        let files = parse_unified_diff(diff);
        assert_eq!(files.len(), 1, "本体行をヘッダ誤認して別ファイル化しない");
        assert_eq!(files[0].new_path, "src/lib.rs");
        assert_eq!(files[0].hunks.len(), 1);
    }

    /// unified diff 仕様違反 (`count > 0 && start == 0`) の hunk header は reject する。
    /// `@@ -1,0 +0,1 @@` のような malformed hunk header を黙って受理すると、
    /// `extract_changed_new_lines` の `current_new_line > 0` ガードで最初の `+` 行が
    /// 取りこぼされる false negative を起こす。回帰防止。
    #[test]
    fn parse_hunk_header_rejects_zero_start_with_positive_count() {
        // new_count > 0 で new_start = 0 は仕様違反
        assert!(parse_hunk_header("@@ -1,0 +0,1 @@").is_none());
        // old_count > 0 で old_start = 0 も仕様違反
        assert!(parse_hunk_header("@@ -0,1 +1,0 @@").is_none());
        // pure-delete (new_count=0, new_start=0) は正当
        assert!(parse_hunk_header("@@ -1,3 +0,0 @@").is_some());
        // pure-add (old_count=0, old_start=0) は正当
        assert!(parse_hunk_header("@@ -0,0 +1,3 @@").is_some());
    }

    /// extract_changed_new_lines も hunk 本体の `+++ b/...` 行を追加行として数え、
    /// ファイルヘッダと誤認しない。
    #[test]
    fn extract_changed_new_lines_body_line_looks_like_header() {
        // target=foo.rs。hunk old=1,new=2。本体に `+++ b/x` (追加行コンテンツ `++ b/x`)。
        let diff = "--- a/foo.rs\n+++ b/foo.rs\n@@ -1,1 +1,2 @@\n existing\n+++ b/x\n";
        let changed = extract_changed_new_lines(diff, "foo.rs");
        // new_start=1: line1 ' existing'(context), line2 '+++ b/x'(added → 0-indexed 1)
        let mut sorted: Vec<_> = changed.into_iter().collect();
        sorted.sort();
        assert_eq!(sorted, vec![1]);
    }

    /// quotepath 形式でクォートされた旧側ヘッダ (`--- "a/..."`) を直前ファイルへ合流させない。
    /// 未 flush のまま後続の `+++ /dev/null` を拾うと直前ファイルの new_path が /dev/null に
    /// 化け、削除 hunk まで誤帰属する (回帰防止)。
    #[test]
    fn parse_unified_diff_unrecognized_quoted_old_header_does_not_merge_into_previous_file() {
        // ascii.rs の通常ブロックの後に、非 ASCII 名 (git quotepath) の削除ブロックが続く。
        let diff = r#"--- a/ascii.rs
+++ b/ascii.rs
@@ -1,1 +1,2 @@
 existing
+added
--- "a/\346\227\245\346\234\254\350\252\236.rs"
+++ /dev/null
@@ -1,2 +0,0 @@
-line1
-line2
"#;
        let files = parse_unified_diff(diff);
        assert_eq!(files.len(), 1, "quotepath ブロックはファイル化されない");
        assert_eq!(files[0].old_path, "ascii.rs");
        assert_ne!(
            files[0].new_path, "/dev/null",
            "後続の +++ /dev/null が ascii.rs に合流しない"
        );
        assert_eq!(files[0].new_path, "ascii.rs");
        assert_eq!(files[0].hunks.len(), 1, "hunk は ascii.rs 自身の 1 個だけ");
        // 削除ブロックの hunk (old_count=2) ではなく ascii.rs 自身の hunk であること
        assert_eq!(files[0].hunks[0].old_count, 1);
        assert_eq!(files[0].hunks[0].new_count, 2);
    }

    /// 片側だけ認識できたペア (`--- a/x` + quotepath の `+++ "b/..."`、rename 風) は
    /// files に現れず、後続の正常ブロックは通常どおり解析される。
    #[test]
    fn parse_unified_diff_unrecognized_quoted_new_header_drops_pair() {
        let diff = r#"--- a/ascii.rs
+++ "b/\346\227\245\346\234\254\350\252\236.rs"
@@ -1,1 +1,1 @@
-old line
+new line
--- a/other.rs
+++ b/other.rs
@@ -1,1 +1,2 @@
 keep
+added
"#;
        let files = parse_unified_diff(diff);
        assert_eq!(
            files.len(),
            1,
            "新側ヘッダを認識できないペアは files に現れない"
        );
        assert!(
            files.iter().all(|f| f.old_path != "ascii.rs"),
            "ascii.rs のブロックは別ファイルの hunk と合成されない"
        );
        assert_eq!(files[0].new_path, "other.rs");
        assert_eq!(files[0].hunks.len(), 1);
        assert_eq!(files[0].hunks[0].new_count, 2);
    }

    /// 内容同一の rename だけを hunk 空の `DiffFile` として返す。内容の差分を持つ rename は
    /// `parse_unified_diff` の担当なので返さず、クォートされたパスは復元できないので返さない。
    #[test]
    fn parse_hunkless_renames_returns_only_content_identical_renames() {
        let diff = r#"diff --git a/src/api.ts b/src/api.ts.bak
similarity index 100%
rename from src/api.ts
rename to src/api.ts.bak
diff --git a/src/edited.ts b/src/edited.txt
similarity index 72%
rename from src/edited.ts
rename to src/edited.txt
index 794a7c2..0eb6578 100644
--- a/src/edited.ts
+++ b/src/edited.txt
@@ -1,1 +1,1 @@
-rename from x
+rename to y
diff --git "a/src/q\tx.ts" "b/src/q\tx.bak"
similarity index 100%
rename from "src/q\tx.ts"
rename to "src/q\tx.bak"
diff --git a/lib/a.rs b/lib/b.rs
similarity index 100%
rename from lib/a.rs
rename to lib/b.rs
"#;
        let renames: Vec<(String, String, usize)> = parse_hunkless_renames(diff)
            .into_iter()
            .map(|f| (f.old_path, f.new_path, f.hunks.len()))
            .collect();
        assert_eq!(
            renames,
            vec![
                ("src/api.ts".to_string(), "src/api.ts.bak".to_string(), 0),
                ("lib/a.rs".to_string(), "lib/b.rs".to_string(), 0),
            ]
        );
        // 対照: 内容同一の rename は `parse_unified_diff` には現れない (既存の前提を変えない)。
        let files = parse_unified_diff(diff);
        assert_eq!(files.len(), 1, "{files:?}");
        assert_eq!(files[0].new_path, "src/edited.txt");
    }
}
