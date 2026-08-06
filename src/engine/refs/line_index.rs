//! 行コンテキスト抽出と位置計算。
//!
//! ファイル単位で改行位置の索引を 1 度だけ構築し、参照件数 M に対し
//! per-call O(filesize) → O(M + filesize) に短縮する。埋め込み領域 (Angular
//! テンプレート / PHPUnit DocBlock / bash trap) の「領域内 offset → ファイル絶対位置」
//! 変換もここに集約する。

/// `LineIndex` を共有して指定行を O(1) で取り出す lexer 経路向け実装。
/// 1 ファイル内で M 件の参照を処理する場合、O(M × filesize) → O(M + filesize) に削減する。
pub(crate) fn extract_line_context_bytes_indexed(
    source: &[u8],
    index: &LineIndex,
    line_0idx: usize,
) -> String {
    // minified/生成コードの巨大行によるメモリ・出力爆発を防ぐため 256B で切り詰める
    // (tree-sitter 経路の extract_line_context と同じ上限)。
    const MAX_CTX: usize = 256;
    let Some((start, end)) = index.line_bounds(source.len(), line_0idx) else {
        return String::new();
    };
    let line = std::str::from_utf8(&source[start..end])
        .unwrap_or("")
        .trim_end_matches('\r');
    if line.len() <= MAX_CTX {
        line.to_string()
    } else {
        format!("{}...", &line[..line.floor_char_boundary(MAX_CTX)])
    }
}

/// 1 ファイルの改行位置 (0-indexed 行頭 byte offset) をキャッシュする索引。
/// `extract_line_context*` を O(filesize) の per-call 走査から O(1) の lookup に
/// 切り替えるため、ファイル単位で 1 度だけ構築して visitor 群に貸し出す。
pub(crate) struct LineIndex {
    /// `line_starts[i]` は 0-indexed の i 行目の先頭 byte offset。終端番兵は持たない。
    line_starts: Vec<u32>,
}

impl LineIndex {
    pub(crate) fn new(source: &[u8]) -> Self {
        // 1 行平均 ~32B 想定で初期 capacity を見積もる
        let mut line_starts = Vec::with_capacity(source.len() / 32 + 1);
        line_starts.push(0u32);
        // 100MB のファイルサイズ上限 (parser::MAX_FILE_SIZE) ≪ u32::MAX のため u32 で十分。
        for nl in memchr::memchr_iter(b'\n', source) {
            // 改行直後の byte offset が次行先頭
            line_starts.push((nl + 1) as u32);
        }
        Self { line_starts }
    }

    /// byte offset を `(row, col)` (0-indexed, byte col) に変換する。
    /// 行頭索引を二分探索するため 1 件あたり O(log 行数)。
    /// `byte` が `source_len` を超える場合は末尾にクランプする。
    pub(crate) fn row_col(&self, source_len: usize, byte: usize) -> (usize, usize) {
        let limit = byte.min(source_len);
        // line_starts は昇順かつ先頭が 0 なので、limit 以下の要素は必ず 1 つ以上ある。
        let row = self
            .line_starts
            .partition_point(|&start| start as usize <= limit)
            - 1;
        (row, limit - self.line_starts[row] as usize)
    }

    /// 指定行の本体 byte 範囲 `[start, end)` を返す。末尾の `\n` は含めない。
    /// 行が存在しない場合は `None`。
    pub(crate) fn line_bounds(&self, source_len: usize, row: usize) -> Option<(usize, usize)> {
        let start = *self.line_starts.get(row)? as usize;
        if start > source_len {
            return None;
        }
        // 次の行頭から `\n` 1 バイトを差し引いた位置が現行の末尾。
        // 最終行 (番兵なし) は source 末尾まで。
        let end = self
            .line_starts
            .get(row + 1)
            .map(|&n| (n as usize).saturating_sub(1).min(source_len))
            .unwrap_or(source_len);
        Some((start, end))
    }
}

/// `text` 内の byte offset を `(row, col)` (0-indexed, byte col) に変換する。
///
/// 同じ `text` から複数箇所引く場合は [`LineIndex`] を 1 度作って
/// [`LineIndex::row_col`] を使う (per-call の O(offset) 走査を避けられる)。
pub(crate) fn byte_offset_to_row_col(text: &str, byte_offset: usize) -> (usize, usize) {
    let bytes = text.as_bytes();
    let limit = byte_offset.min(bytes.len());
    let mut row = 0;
    let mut line_start = 0;
    for nl in memchr::memchr_iter(b'\n', &bytes[..limit]) {
        row += 1;
        line_start = nl + 1;
    }
    (row, limit - line_start)
}

/// 埋め込み領域内の相対位置を、ファイル全体での絶対位置へ変換する。
///
/// `base` は領域先頭 (相対 `(0, 0)`) が対応するファイル上の `(row, col)`。
/// 領域 1 行目だけは領域先頭が行頭ではないので base 列を加算し、2 行目以降は
/// 行頭がファイルの行頭と一致するため相対列をそのまま使う。この規約は Angular
/// inline template / PHPUnit DocBlock・attribute / bash trap handler で共通。
pub(crate) fn absolute_position(base: (usize, usize), rel: (usize, usize)) -> (usize, usize) {
    let (base_row, base_col) = base;
    let (rel_row, rel_col) = rel;
    if rel_row == 0 {
        (base_row, base_col + rel_col)
    } else {
        (base_row + rel_row, rel_col)
    }
}

/// `LineIndex` を共有して指定行を O(1) で取り出す tree-sitter 経路向け実装。
/// minified/生成コードの巨大行によるメモリ爆発を防ぐため 256B で切り詰める。
/// 1 ファイル内で M 件の参照を処理する場合、O(M × filesize) → O(M + filesize) に削減する。
pub(crate) fn extract_line_context_indexed(source: &[u8], index: &LineIndex, row: usize) -> String {
    const MAX_CTX: usize = 256;
    let Some((start, end)) = index.line_bounds(source.len(), row) else {
        return String::new();
    };
    // 必要な範囲のみ UTF-8 変換する（失敗時は空コンテキストを返す）
    let line = std::str::from_utf8(&source[start..end])
        .unwrap_or("")
        .trim();
    if line.len() <= MAX_CTX {
        line.to_string()
    } else {
        // UTF-8 境界で安全に切り詰める
        let truncated = &line[..line.floor_char_boundary(MAX_CTX)];
        format!("{truncated}...")
    }
}

pub(crate) fn context_column(column: usize, source: &[u8], index: &LineIndex, row: usize) -> usize {
    column.saturating_sub(line_trim_start_offset(source, index, row))
}

fn line_trim_start_offset(source: &[u8], index: &LineIndex, row: usize) -> usize {
    let Some((start, end)) = index.line_bounds(source.len(), row) else {
        return 0;
    };
    let Ok(line) = std::str::from_utf8(&source[start..end]) else {
        return 0;
    };
    line.len() - line.trim_start().len()
}
