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
    /// コンテキスト行 (` ` または空行)。先頭の空白 1 文字を除いた内容を保持する
    /// (空行として出力された context は空文字列)。
    Context(&'a str),
    /// `\ No newline at end of file` 等の metadata 行 (行数に数えない)。
    Metadata,
}

/// hunk ヘッダで宣言された old/new の残り行数を追跡し、本体行を消費する。
///
/// 本体を消費し切る (`is_complete`) まではファイル/hunk ヘッダ判定を行わないことで、
/// 削除/追加行のコンテンツが `--- a/...` / `+++ b/...` の形でも誤認しない
/// (hunk 途中で次ファイルヘッダと誤判定し以降の本体が脱落する false negative の防止)。
/// diff を行ごとに読む処理は [`DiffLines`] を通し、この状態を個別に持たない。
struct HunkProgress {
    old_remaining: usize,
    new_remaining: usize,
}

impl HunkProgress {
    fn new(hunk: &HunkInfo) -> Self {
        Self {
            old_remaining: hunk.old_count,
            new_remaining: hunk.new_count,
        }
    }

    /// 本体 1 行を消費して old/new の残数を減らし、行種別を返す。
    fn consume<'a>(&mut self, line: &'a str) -> HunkBodyLine<'a> {
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
                HunkBodyLine::Context(line.strip_prefix(' ').unwrap_or(line))
            }
        }
    }

    fn is_complete(&self) -> bool {
        self.old_remaining == 0 && self.new_remaining == 0
    }
}

/// unified diff の 1 行を、hunk の本体とそれ以外に分けて分類したもの ([`DiffLines`] が返す)。
pub(crate) enum DiffLine<'a> {
    /// hunk の外の `--- ` で始まる行 (旧側のファイルヘッダ)。
    OldHeader(&'a str),
    /// hunk の外の `+++ ` で始まる行 (新側のファイルヘッダ)。
    NewHeader(&'a str),
    /// hunk の外の、解釈できた `@@ ` 行。続く行は宣言した行数を消費し切るまで本体になる。
    Hunk(HunkInfo),
    /// hunk の本体の行。
    Body(HunkBodyLine<'a>),
    /// hunk の外のそれ以外の行 (`diff --git`・`index`・解釈できない `@@ ` など)。
    Other,
}

/// unified diff を先頭から 1 行ずつ [`DiffLine`] に分類する。
///
/// hunk ヘッダで宣言した行数を消費し切るまでは、`--- a/...` / `+++ b/...` / `@@ ` に見える行も
/// 本体として扱う (削除・追加した行の内容がヘッダの形でも、次のファイルや hunk と取り違えない)。
/// 行の切り出しは `str::lines` と同じ (`\n` を落とし、その前の `\r` も落とす)。diff を行ごとに
/// 読む処理はこれを通し、ヘッダと本体の見分けを個別に書かない。ファイルの区間ごとに読むなら
/// [`FileDiffLines`] を使う。
pub(crate) struct DiffLines<'a> {
    rest: std::str::SplitInclusive<'a, char>,
    /// 次の行の先頭のバイト位置。
    offset: usize,
    /// 直前に返した行の先頭のバイト位置。
    line_start: usize,
    active_hunk: Option<HunkProgress>,
}

impl<'a> DiffLines<'a> {
    pub(crate) fn new(input: &'a str) -> Self {
        Self {
            rest: input.split_inclusive('\n'),
            offset: 0,
            line_start: 0,
            active_hunk: None,
        }
    }

    /// 直前に返した行の、入力の先頭からのバイト位置。
    pub(crate) fn line_start(&self) -> usize {
        self.line_start
    }

    /// hunk の本体の途中か (入力の終わりで真なら、宣言した行数に届かずに切れた diff)。
    pub(crate) fn in_hunk(&self) -> bool {
        self.active_hunk.is_some()
    }
}

impl<'a> Iterator for DiffLines<'a> {
    type Item = DiffLine<'a>;

    fn next(&mut self) -> Option<DiffLine<'a>> {
        let raw = self.rest.next()?;
        self.line_start = self.offset;
        self.offset += raw.len();
        let line = raw
            .strip_suffix('\n')
            .map_or(raw, |line| line.strip_suffix('\r').unwrap_or(line));
        if let Some(progress) = self.active_hunk.as_mut() {
            let body = progress.consume(line);
            if progress.is_complete() {
                self.active_hunk = None;
            }
            return Some(DiffLine::Body(body));
        }
        Some(if line.starts_with("--- ") {
            DiffLine::OldHeader(line)
        } else if line.starts_with("+++ ") {
            DiffLine::NewHeader(line)
        } else if line.starts_with("@@ ")
            && let Some(hunk) = parse_hunk_header(line)
        {
            self.active_hunk = Some(HunkProgress::new(&hunk));
            DiffLine::Hunk(hunk)
        } else {
            DiffLine::Other
        })
    }
}

/// 1 つのファイル (`+++ b/<path>` の new 側パス) の区間の行 ([`FileDiffLines`] が返す)。
pub(crate) enum FileDiffLine<'a> {
    /// 区間の始まり (`+++ b/<path>`)。同じファイルが diff に複数回現れれば、そのたびに返す
    /// (最後の区間だけを使う呼び出し側は、ここで集めた内容を捨てる)。
    Header,
    Hunk(HunkInfo),
    Body(HunkBodyLine<'a>),
}

/// diff のうち 1 つのファイルの区間の hunk だけを返す。hunk の外の `--- ` と、`+++ b/<path>` 以外の
/// `+++ ` (`+++ /dev/null`・クォートされたパスなど) で区間を閉じる。閉じないと、削除ファイルの
/// 本文の `-` 行が直前の対象ファイルのものとして数えられる。
pub(crate) struct FileDiffLines<'a, 'p> {
    lines: DiffLines<'a>,
    file_path: &'p str,
    in_file: bool,
}

impl<'a, 'p> FileDiffLines<'a, 'p> {
    pub(crate) fn new(input: &'a str, file_path: &'p str) -> Self {
        Self {
            lines: DiffLines::new(input),
            file_path,
            in_file: false,
        }
    }

    /// 対象のファイルの hunk の途中か (入力の終わりで真なら、そのファイルの diff が途中で切れている)。
    pub(crate) fn in_file_hunk(&self) -> bool {
        self.in_file && self.lines.in_hunk()
    }
}

impl<'a> Iterator for FileDiffLines<'a, '_> {
    type Item = FileDiffLine<'a>;

    fn next(&mut self) -> Option<FileDiffLine<'a>> {
        loop {
            match self.lines.next()? {
                DiffLine::Body(body) if self.in_file => return Some(FileDiffLine::Body(body)),
                DiffLine::Hunk(hunk) if self.in_file => return Some(FileDiffLine::Hunk(hunk)),
                DiffLine::OldHeader(_) => self.in_file = false,
                DiffLine::NewHeader(header) => {
                    self.in_file = strip_header_path(header, "+++ b/") == Some(self.file_path);
                    if self.in_file {
                        return Some(FileDiffLine::Header);
                    }
                }
                DiffLine::Body(_) | DiffLine::Hunk(_) | DiffLine::Other => {}
            }
        }
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
/// 区間の境界は [`FileDiffLines`] と同じ [`DiffLines`] の分類で決める: hunk 本体を消費中の行は
/// (ヘッダに見えても) 境界にしない。hunk 外の `--- ` / `+++ ` 行で区間を閉じ、`+++ b/<path>` なら
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
        let mut lines = DiffLines::new(input);
        while let Some(line) = lines.next() {
            let (DiffLine::OldHeader(header) | DiffLine::NewHeader(header)) = line else {
                continue;
            };
            let line_start = lines.line_start();
            if let Some((path, start)) = current.take() {
                ranges_by_path
                    .entry(path)
                    .or_default()
                    .push(start..line_start);
            }
            if let Some(path) = strip_header_path(header, "+++ b/") {
                current = Some((path, line_start));
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
    // 1-indexed: 次に現れる new 側行番号。
    let mut current_new_line: usize = 0;

    for line in FileDiffLines::new(input, file_path) {
        match line {
            FileDiffLine::Header => {}
            FileDiffLine::Hunk(hunk) => {
                // ゼロ幅 hunk (`-U0` の純削除、例 `@@ -5 +4,0 @@`) の `new_start` は
                // 「削除が起きた位置の**直前**にある new 側行」を 1-indexed で指す。gap の規約は
                // 「削除の**直後**に来る new 側行」なので、ここで 1 行進めて座標系を揃える
                // (ゼロ幅 hunk は定義上 `+` 行を持たないため added_lines への影響はない)。
                current_new_line = if hunk.new_count == 0 {
                    hunk.new_start.saturating_add(1)
                } else {
                    hunk.new_start
                };
            }
            FileDiffLine::Body(HunkBodyLine::Added(_)) => {
                if current_new_line > 0 {
                    facts.added_lines.insert(current_new_line - 1);
                }
                current_new_line += 1;
            }
            FileDiffLine::Body(HunkBodyLine::Context(_)) => {
                current_new_line += 1;
            }
            // 削除行は new 側に行を持たないので行番号を進めない。代わりに
            // 「次に現れる new 側行」を gap として記録する。
            FileDiffLine::Body(HunkBodyLine::Removed(_)) => {
                if current_new_line > 0 {
                    facts.deletion_gaps.insert(current_new_line - 1);
                }
            }
            // metadata 行 (`\ No newline at end of file` 等) は行数に数えない。
            FileDiffLine::Body(HunkBodyLine::Metadata) => {}
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

    let mut lines = FileDiffLines::new(input, file_path);
    for line in lines.by_ref() {
        match line {
            FileDiffLine::Header | FileDiffLine::Body(HunkBodyLine::Metadata) => {}
            FileDiffLine::Hunk(hunk) => {
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
            // context 行は先頭の空白 1 文字を落として比べる (空行として出力された context も許す)。
            FileDiffLine::Body(HunkBodyLine::Context(text)) => {
                let source_line = new_lines.get(new_idx)?;
                if !same_line(source_line, text) {
                    return None;
                }
                out.extend_from_slice(source_line);
                out.push(b'\n');
                old_line += 1;
                new_idx += 1;
            }
            FileDiffLine::Body(HunkBodyLine::Added(text)) => {
                if !same_line(new_lines.get(new_idx)?, text) {
                    return None;
                }
                new_idx += 1;
            }
            FileDiffLine::Body(HunkBodyLine::Removed(text)) => {
                removed_lines.insert(old_line);
                out.extend_from_slice(text.as_bytes());
                out.push(b'\n');
                old_line += 1;
            }
        }
    }
    // 対象ファイルの hunk が宣言した行数に届かないまま入力が終わった (途中で切れた diff)。
    // 残りを「変更なし」とみなして組み立てると、切れた後ろの `-` 行 (宣言ヘッダなど) を
    // 失った old 側で判定することになる。
    if lines.in_file_hunk() {
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
    // 1-indexed: 次に出力される new 側行番号
    let mut current_new_line: usize = 0;

    for line in FileDiffLines::new(input, file_path) {
        match line {
            FileDiffLine::Hunk(hunk) => current_new_line = hunk.new_start,
            FileDiffLine::Body(HunkBodyLine::Added(_) | HunkBodyLine::Context(_)) => {
                current_new_line += 1;
            }
            FileDiffLine::Body(HunkBodyLine::Removed(_)) => {
                let pos = current_new_line.saturating_sub(1);
                if pos >= start_line && pos <= end_line {
                    return true;
                }
            }
            FileDiffLine::Header | FileDiffLine::Body(HunkBodyLine::Metadata) => {}
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

    // hunk 本体を消費中は、`--- a/...` / `+++ b/...` に見える行も本体行として扱い、
    // ファイルヘッダ判定へ落とさない (`DiffLines`。削除/追加行のコンテンツが diff ヘッダと
    // 衝突するケースで以降の hunk 本体が脱落する false negative を防ぐ)。
    for line in DiffLines::new(input) {
        match line {
            DiffLine::Body(body) => {
                if current_new_path.as_deref() == Some("/dev/null")
                    && let HunkBodyLine::Removed(removed) = body
                {
                    // 削除ファイルの hunk 内 `-` 行を旧ソース順で蓄積する。
                    current_deleted_lines.extend_from_slice(removed.as_bytes());
                    current_deleted_lines.push(b'\n');
                }
            }
            DiffLine::OldHeader(header) => {
                // 直前のファイル情報を確定
                flush_file(
                    &mut files,
                    &mut current_old_path,
                    &mut current_new_path,
                    &mut current_hunks,
                    &mut current_deleted_lines,
                );
                current_old_path = if let Some(path) = strip_header_path(header, "--- a/") {
                    Some(path.to_string())
                } else if header.starts_with("--- /dev/null") {
                    Some("/dev/null".to_string())
                } else {
                    // 認識できない旧側ヘッダ (quotepath クォート `--- "a/..."` 等)。
                    // 直前ファイルの state を引きずると以降の hunk が誤帰属するため、
                    // flush して当該ファイルを解析対象から外す (fail-safe)。
                    None
                };
            }
            DiffLine::NewHeader(header) => {
                current_new_path = if let Some(path) = strip_header_path(header, "+++ b/") {
                    Some(path.to_string())
                } else if header.starts_with("+++ /dev/null") {
                    Some("/dev/null".to_string())
                } else {
                    // 認識できない新側ヘッダ。片側だけ認識できたペア (`--- a/x` + `+++ "b/..."`) を
                    // 別ファイルの hunk と合成しないよう、新側を未確定に戻す。
                    None
                };
            }
            DiffLine::Hunk(hunk) => current_hunks.push(hunk),
            DiffLine::Other => {}
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
    parse_hunkless(input, false)
}

/// hunk の無い変更: 内容が同じ rename ([`parse_hunkless_renames`]) と、空のファイルの追加・削除
/// (`new file mode` / `deleted file mode` だけのブロック。`parse_unified_diff` には現れない)。追加は
/// 旧パスを、削除は新パスを `/dev/null` にする。空のファイルの出入りでも、TypeScript の指定子の
/// 解決先は変わる (Issue #288)。Git が C 形式でクォートしたパス (`core.quotePath`) も戻す。
pub fn parse_hunkless_changes(input: &str) -> Vec<DiffFile> {
    parse_hunkless(input, true)
}

fn parse_hunkless(input: &str, empty_files: bool) -> Vec<DiffFile> {
    #[derive(Default)]
    struct Block {
        from: Option<String>,
        to: Option<String>,
        /// `diff --git a/<p> b/<p>` の見出しの、旧新で同じパス。
        path: Option<String>,
        added: bool,
        deleted: bool,
        has_content: bool,
    }
    impl Block {
        fn finish(self, out: &mut Vec<DiffFile>, empty_files: bool) {
            if self.has_content {
                return;
            }
            let paths = match (self.from, self.to, self.path) {
                (Some(old_path), Some(new_path), _) => Some((old_path, new_path)),
                (None, None, Some(path)) if empty_files && self.added => {
                    Some(("/dev/null".to_string(), path))
                }
                (None, None, Some(path)) if empty_files && self.deleted => {
                    Some((path, "/dev/null".to_string()))
                }
                _ => None,
            };
            if let Some((old_path, new_path)) = paths {
                out.push(DiffFile {
                    old_path,
                    new_path,
                    hunks: Vec::new(),
                    deleted_old_source: None,
                });
            }
        }
    }
    // 内容同一の rename だけを返すときは、クォートしたパスを戻さない (従来どおり)。
    let unquoted = |path: &str| {
        if path.starts_with('"') {
            empty_files.then(|| unquote_c_path(path)).flatten()
        } else {
            Some(path.to_string())
        }
    };
    // `diff --git a/<p> b/<p>` (旧新で同じパス) の `<p>`。
    let same_path = |line: &str| -> Option<String> {
        let rest = line.strip_prefix("diff --git ")?;
        if rest.starts_with('"') {
            if !empty_files {
                return None;
            }
            // `"a/<p>" "b/<p>"`
            let (old, new) = split_c_quoted(rest)?;
            let old = unquote_c_path(old)?;
            let new = unquote_c_path(new.strip_prefix(' ')?)?;
            let (old, new) = (old.strip_prefix("a/")?, new.strip_prefix("b/")?);
            return (old == new).then(|| old.to_string());
        }
        let rest = rest.strip_prefix("a/")?;
        let length = rest.len().checked_sub(" b/".len())?;
        if length % 2 != 0 {
            return None;
        }
        let (old, new) = rest.split_at_checked(length / 2)?;
        let new = new.strip_prefix(" b/")?;
        (old == new).then(|| old.to_string())
    };

    let mut out = Vec::new();
    let mut block = Block::default();
    for line in input.lines() {
        if line.starts_with("diff --git ") {
            std::mem::take(&mut block).finish(&mut out, empty_files);
            block.path = same_path(line);
        } else if line.starts_with("new file mode ") {
            block.added = true;
        } else if line.starts_with("deleted file mode ") {
            block.deleted = true;
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
    block.finish(&mut out, empty_files);
    out
}

/// diff の 1 ブロックの片側のパス ([`scan_blocks`])。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BlockPath {
    /// 確定したパス (`a/`・`b/` の接頭辞を外したもの)。
    Known(String),
    /// その側には無い (追加・削除)。
    Absent,
    /// 確定できない。
    Unknown,
}

/// diff の 1 ブロックの片側の mode ([`scan_blocks`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlockMode {
    Known(u32),
    Absent,
    /// diff に現れない (内容が同じ rename・copy や git 以外の unified diff)。
    Unknown,
}

/// diff の 1 ブロックの内容の差分の形 ([`scan_blocks`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlockContent {
    /// hunk で旧新を比べられる。
    Hunks,
    /// binary などで旧新を比べられない。
    Binary,
    /// 内容の差分が無い (内容が同じ rename・mode だけの変更・空のファイルの出入り)。
    Unchanged,
}

/// diff の 1 ブロック。`diff --git` の見出しから次の見出しまでで、git 以外の unified diff では
/// `--- `/`+++ ` の組から始まる。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiffBlock {
    pub(crate) old_path: BlockPath,
    pub(crate) new_path: BlockPath,
    pub(crate) old_mode: BlockMode,
    pub(crate) new_mode: BlockMode,
    pub(crate) content: BlockContent,
}

/// [`scan_blocks`] の結果。
pub(crate) struct BlockScan {
    pub(crate) blocks: Vec<DiffBlock>,
    /// 入力の全ての行を、対応する形として読み切れたか。偽なら、一覧に無い変更があり得る。
    pub(crate) complete: bool,
}

/// diff の全てのブロックを、hunk の有無・binary・パスのクォートを問わず列挙する。
///
/// `parse_unified_diff` (hunk のあるファイル) と `parse_hunkless_changes` (hunk の無い rename と
/// 空のファイル) は解析の対象を選ぶためのもので、binary のブロック・mode だけの変更・クォートした
/// 見出しを返さない。差分が何を変えたかを漏れなく確かめる側 (Cargo の構成の変更の判定) はこれを
/// 使う (Issue #377)。hunk の本文は [`DiffLines`] で見出しと見分け、本文に見出しのような行が
/// あっても読み違えない。対応していない行 (`diff --cc`・解釈できない `@@ `・接頭辞の無い本文の行・
/// どのブロックにも属さない行) や、宣言した行数に届かずに切れた hunk があれば `complete` を偽にする。
pub(crate) fn scan_blocks(input: &str) -> BlockScan {
    let mut scan = BlockScan {
        blocks: Vec::new(),
        complete: true,
    };
    let mut current: Option<BlockBuilder> = None;
    let mut lines = DiffLines::new(input);
    while let Some(line) = lines.next() {
        let raw = raw_line(input, lines.line_start());
        match line {
            DiffLine::Body(_) => {
                // 宣言した行数を消費し切るまで、接頭辞の無い行も本文として読まれる。次のブロックの
                // 見出しを飲み込んでいるかもしれないので、本文の形でなければ読み切れない。
                if !matches!(
                    raw.as_bytes().first(),
                    None | Some(b' ' | b'+' | b'-' | b'\\')
                ) {
                    scan.complete = false;
                }
                match current.as_mut() {
                    Some(block) => block.hunks = true,
                    None => scan.complete = false,
                }
            }
            DiffLine::Hunk(_) => match current.as_mut() {
                Some(block) if block.new_header.is_some() => block.hunks = true,
                _ => scan.complete = false,
            },
            DiffLine::OldHeader(header) => {
                if !current
                    .as_ref()
                    .is_some_and(|block| block.git && block.old_header.is_none())
                {
                    // git 以外の unified diff のブロック。
                    if let Some(block) = current.take() {
                        scan.blocks.push(block.finish());
                    }
                    current = Some(BlockBuilder::default());
                }
                if let Some(block) = current.as_mut() {
                    block.old_header = Some(header_side(header, "--- ", "a/"));
                }
            }
            DiffLine::NewHeader(header) => match current.as_mut() {
                Some(block) if block.old_header.is_some() && block.new_header.is_none() => {
                    block.new_header = Some(header_side(header, "+++ ", "b/"));
                }
                _ => scan.complete = false,
            },
            DiffLine::Other => {
                if let Some(rest) = raw.strip_prefix("diff --git ") {
                    if let Some(block) = current.take() {
                        scan.blocks.push(block.finish());
                    }
                    current = Some(BlockBuilder {
                        git: true,
                        header: Some(git_header_paths(rest)),
                        ..BlockBuilder::default()
                    });
                } else if raw.is_empty()
                    && !current.as_ref().is_some_and(|block| block.in_binary_patch)
                {
                    // hunk の外の空行は何も変えない。
                } else if raw.starts_with("\\ ")
                    && current.as_ref().is_some_and(|block| block.hunks)
                {
                    // 最後の行に改行が無い印 (`\ No newline at end of file`)。hunk の行数を数え
                    // 終えた後に来る。
                } else if !current
                    .as_mut()
                    .is_some_and(|block| block.read_extended_header(raw))
                {
                    scan.complete = false;
                }
            }
        }
    }
    if lines.in_hunk() {
        scan.complete = false;
    }
    if let Some(block) = current {
        scan.blocks.push(block.finish());
    }
    scan
}

/// `start` から行末 (`\n` の前、`\r\n` なら `\r` の前) までの行。
fn raw_line(input: &str, start: usize) -> &str {
    let rest = &input[start..];
    let line = rest.split_once('\n').map_or(rest, |(line, _)| line);
    line.strip_suffix('\r').unwrap_or(line)
}

/// `--- ` / `+++ ` の見出しの片側。
#[derive(Clone)]
enum HeaderSide {
    Path(String),
    DevNull,
    Unknown,
}

fn header_side(header: &str, marker: &str, prefix: &str) -> HeaderSide {
    let Some(rest) = header.strip_prefix(marker) else {
        return HeaderSide::Unknown;
    };
    if rest == "/dev/null" || rest.starts_with("/dev/null\t") {
        return HeaderSide::DevNull;
    }
    if rest.starts_with('"') {
        // `--- "a/x\ty"` の後ろに TAB とタイムスタンプは付かない (クォートがラベルを閉じる)。
        return match split_c_quoted(rest) {
            Some((quoted, "")) => unquote_c_path(quoted)
                .and_then(|path| path.strip_prefix(prefix).map(str::to_owned))
                .map_or(HeaderSide::Unknown, HeaderSide::Path),
            _ => HeaderSide::Unknown,
        };
    }
    match strip_header_path(header, &format!("{marker}{prefix}")) {
        Some(path) if !path.is_empty() => HeaderSide::Path(path.to_owned()),
        _ => HeaderSide::Unknown,
    }
}

/// `diff --git ` の後ろ (`a/<旧> b/<新>`) の旧新のパス。どちらか、または両方をクォートしてよい。
/// クォートしない旧新が違うパスは、空白を含むと区切りが曖昧なので確定しない (None)。
fn git_header_paths(rest: &str) -> Option<(String, String)> {
    let unquote = |quoted: &str, prefix: &str| {
        unquote_c_path(quoted).and_then(|path| path.strip_prefix(prefix).map(str::to_owned))
    };
    if rest.starts_with('"') {
        let (old, tail) = split_c_quoted(rest)?;
        let old = unquote(old, "a/")?;
        let tail = tail.strip_prefix(' ')?;
        let new = if tail.starts_with('"') {
            match split_c_quoted(tail)? {
                (new, "") => unquote(new, "b/")?,
                _ => return None,
            }
        } else {
            tail.strip_prefix("b/")?.to_owned()
        };
        return Some((old, new));
    }
    // クォートしないパスは `"` を含まない (Git は `"` を含むパスをクォートする)。
    if let Some(quote) = rest.find('"') {
        let old = rest[..quote].strip_suffix(' ')?.strip_prefix("a/")?;
        return match split_c_quoted(&rest[quote..])? {
            (new, "") => Some((old.to_owned(), unquote(new, "b/")?)),
            _ => None,
        };
    }
    let rest = rest.strip_prefix("a/")?;
    let length = rest.len().checked_sub(" b/".len())?;
    if length % 2 != 0 {
        return None;
    }
    let (old, new) = rest.split_at_checked(length / 2)?;
    let new = new.strip_prefix(" b/")?;
    (old == new).then(|| (old.to_owned(), new.to_owned()))
}

/// `rename from <path>` などの拡張ヘッダのパス (接頭辞なし。クォートしてよい)。
fn extended_header_path(path: &str) -> Option<String> {
    if path.starts_with('"') {
        return match split_c_quoted(path)? {
            (quoted, "") => unquote_c_path(quoted),
            _ => None,
        };
    }
    (!path.is_empty()).then(|| path.to_owned())
}

/// `GIT binary patch` の本体の行: `literal <n>` / `delta <n>`、base85 の行 (長さの文字 `A`-`Z`・
/// `a`-`z` と base85 の文字)、区切りの空行。
fn binary_patch_line(line: &str) -> bool {
    if line.is_empty() {
        return true;
    }
    if let Some(size) = line
        .strip_prefix("literal ")
        .or_else(|| line.strip_prefix("delta "))
    {
        return !size.is_empty() && size.bytes().all(|byte| byte.is_ascii_digit());
    }
    let mut bytes = line.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_alphabetic())
        && line.len() > 1
        && bytes.all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'('
                        | b')'
                        | b'*'
                        | b'+'
                        | b'-'
                        | b';'
                        | b'<'
                        | b'='
                        | b'>'
                        | b'?'
                        | b'@'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'{'
                        | b'|'
                        | b'}'
                        | b'~'
                )
        })
}

fn parse_mode(text: &str) -> Option<u32> {
    (text.len() == 6 && text.bytes().all(|byte| (b'0'..=b'7').contains(&byte)))
        .then(|| u32::from_str_radix(text, 8).ok())
        .flatten()
}

/// [`scan_blocks`] が 1 ブロックの間に集める事実。
#[derive(Default)]
struct BlockBuilder {
    git: bool,
    /// `diff --git` の見出しの旧新 (None は確定できない)。
    header: Option<Option<(String, String)>>,
    /// `rename from` / `copy from` (None は読めない)。
    from: Option<Option<String>>,
    to: Option<Option<String>>,
    old_header: Option<HeaderSide>,
    new_header: Option<HeaderSide>,
    old_mode: Option<Option<u32>>,
    new_mode: Option<Option<u32>>,
    index_mode: Option<Option<u32>>,
    added: Option<Option<u32>>,
    deleted: Option<Option<u32>>,
    hunks: bool,
    binary: bool,
    /// `GIT binary patch` の本体の途中。
    in_binary_patch: bool,
    /// 同じ事実を 2 度読んだなど、ブロックの形が崩れている。
    malformed: bool,
}

impl BlockBuilder {
    /// git のブロックの拡張ヘッダ (と `GIT binary patch` の本体) を 1 行読む。対応しない行なら偽。
    fn read_extended_header(&mut self, line: &str) -> bool {
        if self.in_binary_patch {
            return binary_patch_line(line);
        }
        if !self.git || self.old_header.is_some() {
            return false;
        }
        let set = |slot: &mut Option<Option<u32>>, malformed: &mut bool, text: &str| {
            *malformed |= slot.replace(parse_mode(text)).is_some();
        };
        if let Some(mode) = line.strip_prefix("old mode ") {
            set(&mut self.old_mode, &mut self.malformed, mode);
        } else if let Some(mode) = line.strip_prefix("new mode ") {
            set(&mut self.new_mode, &mut self.malformed, mode);
        } else if let Some(mode) = line.strip_prefix("new file mode ") {
            set(&mut self.added, &mut self.malformed, mode);
        } else if let Some(mode) = line.strip_prefix("deleted file mode ") {
            set(&mut self.deleted, &mut self.malformed, mode);
        } else if let Some(index) = line.strip_prefix("index ") {
            let (range, mode) = match index.split_once(' ') {
                Some((range, mode)) => (range, Some(mode)),
                None => (index, None),
            };
            if !range.split_once("..").is_some_and(|(old, new)| {
                [old, new]
                    .iter()
                    .all(|oid| !oid.is_empty() && oid.bytes().all(|byte| byte.is_ascii_hexdigit()))
            }) {
                return false;
            }
            let mode = match mode {
                Some(mode) => match parse_mode(mode) {
                    Some(mode) => Some(mode),
                    None => return false,
                },
                None => None,
            };
            self.malformed |= self.index_mode.replace(mode).is_some();
        } else if let Some(path) = line
            .strip_prefix("rename from ")
            .or_else(|| line.strip_prefix("copy from "))
        {
            self.malformed |= self.from.replace(extended_header_path(path)).is_some();
        } else if let Some(path) = line
            .strip_prefix("rename to ")
            .or_else(|| line.strip_prefix("copy to "))
        {
            self.malformed |= self.to.replace(extended_header_path(path)).is_some();
        } else if line.starts_with("similarity index ") || line.starts_with("dissimilarity index ")
        {
        } else if line.starts_with("Binary files ") && line.ends_with(" differ") {
            self.binary = true;
        } else if line == "GIT binary patch" {
            self.binary = true;
            self.in_binary_patch = true;
        } else {
            return false;
        }
        true
    }

    fn finish(self) -> DiffBlock {
        let added = self.added.is_some();
        let deleted = self.deleted.is_some();
        let malformed = self.malformed || added && deleted;
        let header = self.header.flatten();
        let side = |header_path: Option<&String>,
                    extended: &Option<Option<String>>,
                    file_header: &Option<HeaderSide>,
                    absent: bool| {
            if malformed {
                return BlockPath::Unknown;
            }
            let mut value: Option<BlockPath> = None;
            let mut agree = |candidate: BlockPath| {
                value = Some(match value.take() {
                    None => candidate,
                    Some(existing) if existing == candidate => existing,
                    Some(_) => BlockPath::Unknown,
                });
            };
            if absent {
                agree(BlockPath::Absent);
            } else if let Some(path) = header_path {
                // 追加・削除の見出しは、無い側にも同じパスを書く。
                agree(BlockPath::Known(path.clone()));
            }
            match extended {
                Some(Some(path)) => agree(BlockPath::Known(path.clone())),
                Some(None) => agree(BlockPath::Unknown),
                None => {}
            }
            match file_header {
                Some(HeaderSide::Path(path)) => agree(BlockPath::Known(path.clone())),
                Some(HeaderSide::DevNull) => agree(BlockPath::Absent),
                Some(HeaderSide::Unknown) => agree(BlockPath::Unknown),
                None => {}
            }
            value.unwrap_or(BlockPath::Unknown)
        };
        let old_path = side(
            header.as_ref().map(|(old, _)| old),
            &self.from,
            &self.old_header,
            added,
        );
        let new_path = side(
            header.as_ref().map(|(_, new)| new),
            &self.to,
            &self.new_header,
            deleted,
        );
        let mode = |line: Option<Option<u32>>, file: Option<Option<u32>>, absent: bool| {
            if absent {
                return BlockMode::Absent;
            }
            match line.or(file).or(self.index_mode) {
                Some(Some(mode)) => BlockMode::Known(mode),
                _ => BlockMode::Unknown,
            }
        };
        DiffBlock {
            old_mode: mode(self.old_mode, self.deleted, added),
            new_mode: mode(self.new_mode, self.added, deleted),
            old_path,
            new_path,
            content: if self.binary {
                BlockContent::Binary
            } else if self.hunks {
                BlockContent::Hunks
            } else {
                BlockContent::Unchanged
            },
        }
    }
}

/// `text` の先頭の C 形式でクォートした文字列 (`"…"`) と、その後ろ。閉じていなければ None。
fn split_c_quoted(text: &str) -> Option<(&str, &str)> {
    let bytes = text.as_bytes();
    if bytes.first() != Some(&b'"') {
        return None;
    }
    let mut index = 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'"' => return Some((&text[..=index], &text[index + 1..])),
            _ => index += 1,
        }
    }
    None
}

/// Git が C 形式でクォートしたパス (`"src/\346\251\213.ts"`) を戻す。形が違えば None。
fn unquote_c_path(text: &str) -> Option<String> {
    let inner = text.strip_prefix('"')?.strip_suffix('"')?;
    let mut bytes = Vec::with_capacity(inner.len());
    let mut iter = inner.bytes();
    while let Some(byte) = iter.next() {
        if byte != b'\\' {
            bytes.push(byte);
            continue;
        }
        let escaped = match iter.next()? {
            b'a' => 0x07,
            b'b' => 0x08,
            b't' => b'\t',
            b'n' => b'\n',
            b'v' => 0x0b,
            b'f' => 0x0c,
            b'r' => b'\r',
            b'"' => b'"',
            b'\\' => b'\\',
            first @ b'0'..=b'3' => {
                let mut value = u32::from(first - b'0');
                for _ in 0..2 {
                    let digit = iter.next()?;
                    if !(b'0'..=b'7').contains(&digit) {
                        return None;
                    }
                    value = value * 8 + u32::from(digit - b'0');
                }
                u8::try_from(value).ok()?
            }
            _ => return None,
        };
        bytes.push(escaped);
    }
    String::from_utf8(bytes).ok()
}

/// `"@@ -10,5 +10,8 @@"` や `"@@ -10,5 +10,8 @@ fn foo()"` の hunk ヘッダを解析する。
fn parse_hunk_header(line: &str) -> Option<HunkInfo> {
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
    fn reconstruct_old_source_strictly_reverses_multiple_hunks() {
        let diff = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,2 +1,2 @@\n one\n-two\n+changed-two\n@@ -4,2 +4,2 @@\n four\n-five\n+changed-five\n";
        let new = b"one\nchanged-two\nthree\nfour\nchanged-five\n";
        assert_eq!(
            reconstruct_old_source(diff, "src/lib.rs", new).map(|old| old.source),
            Some(b"one\ntwo\nthree\nfour\nfive\n".to_vec())
        );
    }

    #[test]
    fn reconstruct_old_source_rejects_mismatched_new_content() {
        let diff = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+expected\n";
        assert!(reconstruct_old_source(diff, "src/lib.rs", b"different\n").is_none());
    }

    #[test]
    fn reconstruct_old_source_handles_zero_width_new_hunk() {
        let diff = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -2 +1,0 @@\n-two\n";
        assert_eq!(
            reconstruct_old_source(diff, "src/lib.rs", b"one\nthree\n").map(|old| old.source),
            Some(b"one\ntwo\nthree\n".to_vec())
        );
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

    /// 空のファイルの追加・削除 (hunk も `---`/`+++` も無いブロック) は、hunk の無い変更として
    /// 返す (Issue #288)。中身のある追加・mode だけの変更は返さない。C 形式でクォートしたパスは
    /// 戻す。
    #[test]
    fn parse_hunkless_changes_returns_empty_file_additions_and_deletions() {
        let diff = r#"diff --git a/src/empty.ts b/src/empty.ts
new file mode 100644
index 0000000..e69de29
diff --git a/src/gone b/c.ts b/src/gone b/c.ts
deleted file mode 100644
index e69de29..0000000
diff --git a/src/full.ts b/src/full.ts
new file mode 100644
index 0000000..3b18e51
--- /dev/null
+++ b/src/full.ts
@@ -0,0 +1 @@
+export {};
diff --git a/run.sh b/run.sh
old mode 100644
new mode 100755
diff --git "a/src/q\tx.ts" "b/src/q\tx.ts"
new file mode 100644
index 0000000..e69de29
diff --git a/lib/a.rs b/lib/b.rs
similarity index 100%
rename from lib/a.rs
rename to lib/b.rs
diff --git "a/src/\346\251\213.ts" "b/src/\346\251\213.ts"
deleted file mode 100644
index e69de29..0000000
"#;
        let changes: Vec<(String, String)> = parse_hunkless_changes(diff)
            .into_iter()
            .map(|f| (f.old_path, f.new_path))
            .collect();
        assert_eq!(
            changes,
            vec![
                ("/dev/null".to_string(), "src/empty.ts".to_string()),
                ("src/gone b/c.ts".to_string(), "/dev/null".to_string()),
                ("/dev/null".to_string(), "src/q\tx.ts".to_string()),
                ("lib/a.rs".to_string(), "lib/b.rs".to_string()),
                ("src/\u{6a4b}.ts".to_string(), "/dev/null".to_string()),
            ]
        );
        // 内容同一の rename だけを返す既存の関数は変えない。
        assert_eq!(parse_hunkless_renames(diff).len(), 1);
    }

    /// diff を行ごとに読む処理 (区間の索引・変更行・削除位置・旧版の復元・ファイル一覧) の、
    /// 行の分類と区間の扱いを固定する。
    ///
    /// 同じパスの再登場 (最後の区間は hunk 無し) / hunk 完了直後の metadata / 片側だけクォート
    /// されたヘッダー / CRLF / 対象外のファイルの切れた hunk を 1 つの diff に混ぜる。
    #[test]
    fn diff_readers_share_line_classification_and_sections() {
        let diff = concat!(
            "--- a/a.rs\n",
            "+++ b/a.rs\n",
            "@@ -1,2 +1,2 @@\n",
            "-fn a() {}\r\n",
            "+fn a(x: u8) {}\r\n",
            " fn keep() {}\n",
            // hunk の行数を消費し切った後の metadata は hunk の外の行として読み飛ばす。
            "\\ No newline at end of file\n",
            "--- a/q.rs\n",
            // 認識できない新側ヘッダーは区間を閉じ、どのファイルの区間も開かない。
            "+++ \"b/q\\tx.rs\"\n",
            "@@ -1 +1 @@\n",
            "-fn q() {}\n",
            "+fn q2() {}\n",
            "--- a/a.rs\n",
            "+++ b/a.rs\n",
            "--- a/cut.rs\n",
            "+++ b/cut.rs\n",
            "@@ -1,3 +1,3 @@\n",
            "-fn cut() {}\n",
        );

        let sections = FileSections::split(diff);
        assert_eq!(
            sections.get("a.rs"),
            concat!(
                "+++ b/a.rs\n",
                "@@ -1,2 +1,2 @@\n",
                "-fn a() {}\r\n",
                "+fn a(x: u8) {}\r\n",
                " fn keep() {}\n",
                "\\ No newline at end of file\n",
                "+++ b/a.rs\n",
            )
        );
        assert_eq!(
            sections.get("cut.rs"),
            "+++ b/cut.rs\n@@ -1,3 +1,3 @@\n-fn cut() {}\n"
        );
        assert!(sections.get("q\tx.rs").is_empty());

        let a = extract_changed_line_facts(diff, "a.rs");
        assert_eq!(a.added_lines, HashSet::from([0]));
        assert_eq!(a.deletion_gaps, HashSet::from([0]));
        let cut = extract_changed_line_facts(diff, "cut.rs");
        assert!(cut.added_lines.is_empty());
        assert_eq!(cut.deletion_gaps, HashSet::from([0]));
        assert!(has_deletion_in_new_range(diff, "a.rs", 0, 0));
        assert!(!has_deletion_in_new_range(diff, "a.rs", 1, 5));

        // 復元は CRLF の new 側と比べ、対象の区間の切れた hunk だけを失敗にする。
        let rec = reconstruct_old_source(diff, "a.rs", b"fn a(x: u8) {}\r\nfn keep() {}")
            .expect("a.rs は復元できる");
        assert_eq!(rec.source, b"fn a() {}\nfn keep() {}\n");
        assert_eq!(rec.removed_lines, HashSet::from([0]));
        assert!(reconstruct_old_source(diff, "cut.rs", b"x\ny\nz\n").is_none());

        let files: Vec<_> = parse_unified_diff(diff)
            .into_iter()
            .map(|file| {
                let hunks: Vec<_> = file
                    .hunks
                    .iter()
                    .map(|h| (h.old_start, h.old_count, h.new_start, h.new_count))
                    .collect();
                (file.old_path, file.new_path, hunks)
            })
            .collect();
        assert_eq!(
            files,
            vec![
                ("a.rs".to_string(), "a.rs".to_string(), vec![(1, 2, 1, 2)]),
                (
                    "cut.rs".to_string(),
                    "cut.rs".to_string(),
                    vec![(1, 3, 1, 3)]
                ),
            ]
        );
    }

    /// 削除だけのゼロ幅 hunk の位置は、変更行の削除位置 (直後の new 側行) と、削除範囲の近似判定
    /// (直前の new 側行) で意図して違う。同じ規則にまとめない。
    #[test]
    fn zero_width_deletion_positions_differ_between_gaps_and_range_check() {
        let diff = "--- a/z.rs\n+++ b/z.rs\n@@ -5 +4,0 @@\n-gone\n";
        assert_eq!(
            extract_changed_line_facts(diff, "z.rs").deletion_gaps,
            HashSet::from([4])
        );
        assert!(has_deletion_in_new_range(diff, "z.rs", 3, 3));
        assert!(!has_deletion_in_new_range(diff, "z.rs", 4, 4));
    }

    /// 改行の無い末尾の `\r` は `str::lines` と同じく行に残す (CRLF の `\r` だけを落とす)。
    #[test]
    fn trailing_carriage_return_without_newline_stays_in_the_line() {
        let diff = "--- a/r.rs\n+++ b/r.rs\n@@ -1 +1 @@\n-old\n+new\r";
        assert_eq!(
            extract_changed_line_facts(diff, "r.rs").added_lines,
            HashSet::from([0])
        );
        // new 側の `\r` は比較から外すので、diff 側の行に残った `\r` とは一致しない。
        assert!(reconstruct_old_source(diff, "r.rs", b"new\r\n").is_none());
        let crlf = "--- a/r.rs\n+++ b/r.rs\n@@ -1 +1 @@\n-old\r\n+new\r\n";
        let rec = reconstruct_old_source(crlf, "r.rs", b"new\r\n").expect("CRLF は復元できる");
        assert_eq!(rec.source, b"old\n");
    }

    fn known(path: &str) -> BlockPath {
        BlockPath::Known(path.to_owned())
    }

    /// 1 ブロックだけの diff を読み、読み切れたことを確かめてそのブロックを返す。
    fn only_block(diff: &str) -> DiffBlock {
        let scan = scan_blocks(diff);
        assert!(scan.complete, "{diff}");
        assert_eq!(scan.blocks.len(), 1, "{diff}");
        scan.blocks.into_iter().next().unwrap()
    }

    #[test]
    fn scan_blocks_reads_every_block_form() {
        let regular = DiffBlock {
            old_path: known("x.rs"),
            new_path: known("x.rs"),
            old_mode: BlockMode::Known(0o100644),
            new_mode: BlockMode::Known(0o100644),
            content: BlockContent::Hunks,
        };
        assert_eq!(
            only_block(
                "diff --git a/x.rs b/x.rs\nindex aaa..bbb 100644\n--- a/x.rs\n+++ b/x.rs\n@@ -1 +1 @@\n-a\n+b\n"
            ),
            regular
        );
        // 本文に見出しのような行があっても、宣言した行数の間は本文として読む。
        assert_eq!(
            only_block(
                "diff --git a/x.rs b/x.rs\nindex aaa..bbb 100644\n--- a/x.rs\n+++ b/x.rs\n@@ -1,2 +1,2 @@\n--- a/y.rs\n+++ b/y.rs\n-diff --git a/z b/z\n+diff --git a/w b/w\n\\ No newline at end of file\n"
            ),
            regular
        );
        assert_eq!(
            only_block(
                "diff --git a/x b/x\nnew file mode 100644\nindex 0000000..bbb\n--- /dev/null\n+++ b/x\n@@ -0,0 +1 @@\n+a\n"
            ),
            DiffBlock {
                old_path: BlockPath::Absent,
                new_path: known("x"),
                old_mode: BlockMode::Absent,
                new_mode: BlockMode::Known(0o100644),
                content: BlockContent::Hunks,
            }
        );
        assert_eq!(
            only_block("diff --git a/x b/x\ndeleted file mode 100755\nindex aaa..0000000\n"),
            DiffBlock {
                old_path: known("x"),
                new_path: BlockPath::Absent,
                old_mode: BlockMode::Known(0o100755),
                new_mode: BlockMode::Absent,
                content: BlockContent::Unchanged,
            }
        );
        // 内容が同じ rename は mode が出ない (gitlink の rename と区別できない)。
        assert_eq!(
            only_block(
                "diff --git a/a.md b/b.md\nsimilarity index 100%\nrename from a.md\nrename to b.md\n"
            ),
            DiffBlock {
                old_path: known("a.md"),
                new_path: known("b.md"),
                old_mode: BlockMode::Unknown,
                new_mode: BlockMode::Unknown,
                content: BlockContent::Unchanged,
            }
        );
        assert_eq!(
            only_block(
                "diff --git a/a.md b/b.md\nsimilarity index 80%\nrename from a.md\nrename to b.md\nindex aaa..bbb 100644\n--- a/a.md\n+++ b/b.md\n@@ -1 +1 @@\n-a\n+b\n"
            )
            .new_mode,
            BlockMode::Known(0o100644)
        );
        assert_eq!(
            only_block("diff --git a/x b/x\nold mode 100644\nnew mode 100755\n"),
            DiffBlock {
                old_path: known("x"),
                new_path: known("x"),
                old_mode: BlockMode::Known(0o100644),
                new_mode: BlockMode::Known(0o100755),
                content: BlockContent::Unchanged,
            }
        );
        assert_eq!(
            only_block(
                "diff --git a/b/Cargo.toml b/b/Cargo.toml\nindex aaa..bbb 100644\nBinary files a/b/Cargo.toml and b/b/Cargo.toml differ\n"
            )
            .content,
            BlockContent::Binary
        );
        assert_eq!(
            only_block(
                "diff --git a/x.bin b/x.bin\nindex aaa..bbb 100644\nGIT binary patch\nliteral 5\nMcmZQzWMW|e00093\n\nliteral 0\nHcmV?d00001\n\n"
            )
            .content,
            BlockContent::Binary
        );
        assert_eq!(
            only_block(
                "diff --git a/b b/b\nindex aaa..bbb 160000\n--- a/b\n+++ b/b\n@@ -1 +1 @@\n-Subproject commit aaa\n+Subproject commit bbb-dirty\n"
            )
            .new_mode,
            BlockMode::Known(0o160000)
        );
        // クォートした見出し (片側だけでもよい) は戻す。
        let quoted = only_block(
            "diff --git \"a/x\\ty.rs\" \"b/x\\ty.rs\"\nindex aaa..bbb 100644\n--- \"a/x\\ty.rs\"\n+++ \"b/x\\ty.rs\"\n@@ -1 +1 @@\n-a\n+b\n",
        );
        assert_eq!(
            (quoted.old_path, quoted.new_path),
            (known("x\ty.rs"), known("x\ty.rs"))
        );
        let renamed = only_block(
            "diff --git a/plain.rs \"b/x\\ty.rs\"\nsimilarity index 100%\nrename from plain.rs\nrename to \"x\\ty.rs\"\n",
        );
        assert_eq!(
            (renamed.old_path, renamed.new_path),
            (known("plain.rs"), known("x\ty.rs"))
        );
        // 見出しと拡張ヘッダが食い違うパスは確定しない。
        assert_eq!(
            only_block(
                "diff --git \"a/x\\ty\" \"b/z\"\nsimilarity index 100%\nrename from other\nrename to z\n"
            )
            .old_path,
            BlockPath::Unknown
        );
        // git 以外の unified diff。
        assert_eq!(
            only_block("--- a/x.rs\n+++ b/x.rs\n@@ -1 +1 @@\n-a\n+b\n"),
            DiffBlock {
                old_path: known("x.rs"),
                new_path: known("x.rs"),
                old_mode: BlockMode::Unknown,
                new_mode: BlockMode::Unknown,
                content: BlockContent::Hunks,
            }
        );
        let empty = scan_blocks("");
        assert!(empty.complete && empty.blocks.is_empty());
        let two = scan_blocks(
            "diff --git a/x b/x\nindex aaa..bbb 100644\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\ndiff --git a/y b/y\nold mode 100644\nnew mode 100755\n",
        );
        assert!(two.complete);
        assert_eq!(two.blocks.len(), 2);
    }

    #[test]
    fn scan_blocks_rejects_unsupported_or_truncated_input() {
        for diff in [
            // 結合 diff。
            "diff --cc x.rs\nindex aaa,bbb..ccc\n--- a/x.rs\n+++ b/x.rs\n@@@ -1,1 -1,1 +1,1 @@@\n- a\n -b\n++c\n",
            // 宣言した行数に届かずに切れた hunk。
            "diff --git a/x b/x\nindex aaa..bbb 100644\n--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n-a\n+b\n",
            // 行数の足りない hunk が次のブロックの見出しを本文として飲み込む。
            "diff --git a/x b/x\nindex aaa..bbb 100644\n--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n-a\n+b\ndiff --git a/y b/y\nold mode 100644\nnew mode 100755\n",
            // どのブロックにも属さない行。
            "commit 0123456\n\ndiff --git a/x b/x\nold mode 100644\nnew mode 100755\n",
            // 解釈できない `@@ `。
            "diff --git a/x b/x\nindex aaa..bbb 100644\n--- a/x\n+++ b/x\n@@ broken @@\n",
            // `GIT binary patch` の本体の形でない行。
            "diff --git a/x b/x\nindex aaa..bbb 100644\nGIT binary patch\nliteral 5\ndiff --cc y\n",
            // 対応しない拡張ヘッダ。
            "diff --git a/x b/x\nunknown header\n",
        ] {
            assert!(!scan_blocks(diff).complete, "{diff}");
        }
    }
}
