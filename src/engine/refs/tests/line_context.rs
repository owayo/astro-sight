use super::*;

/// 指定行 (0-indexed) のコンテキスト行を取得する。lexer fallback 経路用の
/// 軽量実装 (tree-sitter の Node API に依存しない)。1 ファイル内で複数行参照する
/// 場合は `extract_line_context_bytes_indexed` を使い `LineIndex` を共有する。
/// 本体経路は indexed 版を直接呼ぶため、この単発 wrapper はテスト専用。
#[cfg(test)]
fn extract_line_context_bytes(source: &[u8], line_0idx: usize) -> String {
    extract_line_context_bytes_indexed(source, &LineIndex::new(source), line_0idx)
}

/// 指定行のソース行をコンテキストとして抽出する。
/// 後方互換のため `LineIndex` を内部で作る単発呼び出し用。1 ファイル内で複数行を
/// 取り出す場合は `extract_line_context_indexed` を使い `LineIndex` を共有する。
/// 本体経路は indexed 版を直接呼ぶため、この単発 wrapper はテスト専用。
#[cfg(test)]
fn extract_line_context(source: &[u8], row: usize) -> String {
    extract_line_context_indexed(source, &LineIndex::new(source), row)
}

/// 指定行のソースが正しく抽出され、前後の空白が除去されることを検証
#[test]
fn extract_line_context_basic() {
    let source = b"line0\n  line1  \nline2";
    let ctx = extract_line_context(source, 1);
    assert_eq!(ctx, "line1");
}

/// 範囲外の行に対して空文字を返すことを検証
#[test]
fn extract_line_context_out_of_range() {
    let source = b"only one line";
    let ctx = extract_line_context(source, 5);
    assert_eq!(ctx, "");
}

/// 改行なしで終わる最終行も正しく抽出できることを検証（memchr 版で新規テスト）
#[test]
fn extract_line_context_final_line_without_newline() {
    let source = b"first\nsecond";
    let ctx = extract_line_context(source, 1);
    assert_eq!(ctx, "second");
}

/// 巨大行を 256 バイト境界で切り詰めることを検証（minified コード防御）
#[test]
fn extract_line_context_truncates_long_line() {
    let long = "a".repeat(500);
    let source = format!("line0\n{long}");
    let ctx = extract_line_context(source.as_bytes(), 1);
    assert!(ctx.ends_with("..."), "256 バイト超は省略記号で終わるべき");
    assert!(ctx.len() <= 256 + 3, "256 バイト + '...' 以内に収まるべき");
}

/// UTF-8 境界で安全に切り詰められることを検証（マルチバイト文字の分割禁止）
#[test]
fn extract_line_context_utf8_boundary_safe() {
    // 「あ」は UTF-8 で 3 バイト。256B 境界を跨ぐ位置に配置する
    let mut long = "a".repeat(254);
    long.push_str("あいうえお");
    let source = format!("x\n{long}");
    let ctx = extract_line_context(source.as_bytes(), 1);
    // UTF-8 境界違反でパニックしないこと
    assert!(ctx.ends_with("..."));
    assert!(std::str::from_utf8(ctx.as_bytes()).is_ok());
}

/// lexer 経路 (extract_line_context_bytes) も巨大行を 256 バイトで切り詰めることを検証。
/// tree-sitter 非対応言語 (Xojo) の minified/生成行によるメモリ・出力爆発を防ぐ。
/// 中間行（改行あり）と最終行（改行なし）の両経路を確認する。
#[test]
fn extract_line_context_bytes_truncates_long_line() {
    let long = "a".repeat(500);
    let source = format!("line0\n{long}\n{long}");
    let ctx_mid = extract_line_context_bytes(source.as_bytes(), 1);
    assert!(
        ctx_mid.ends_with("..."),
        "256 バイト超は省略記号で終わるべき"
    );
    assert!(
        ctx_mid.len() <= 256 + 3,
        "256 バイト + '...' 以内に収まるべき"
    );
    let ctx_last = extract_line_context_bytes(source.as_bytes(), 2);
    assert!(
        ctx_last.ends_with("..."),
        "最終行（改行なし）も切り詰めるべき"
    );
    assert!(ctx_last.len() <= 256 + 3);
    // 通常行（256 バイト以下）は切り詰めない
    let ctx0 = extract_line_context_bytes(source.as_bytes(), 0);
    assert_eq!(ctx0, "line0");
}

/// lexer 経路も UTF-8 境界で安全に切り詰めることを検証（マルチバイト文字の分割禁止）
#[test]
fn extract_line_context_bytes_utf8_boundary_safe() {
    let mut long = "a".repeat(254);
    long.push_str("あいうえお");
    let source = format!("x\n{long}");
    let ctx = extract_line_context_bytes(source.as_bytes(), 1);
    assert!(ctx.ends_with("..."));
    assert!(std::str::from_utf8(ctx.as_bytes()).is_ok());
}

/// `LineIndex::new` が空ソースでも line 0 を空文字として扱えることを検証。
#[test]
fn line_index_handles_empty_source() {
    let source: &[u8] = b"";
    let index = LineIndex::new(source);
    // 空ソースは line_starts = [0] のみで、line 0 の bounds は (0, 0)。
    assert_eq!(index.line_bounds(0, 0), Some((0, 0)));
    // 範囲外行 (>=1) は None。
    assert_eq!(index.line_bounds(0, 1), None);
}

/// `LineIndex` が末尾改行のあるソースで最終空行を追加で含むことを検証。
/// `b"a\n"` は通常 1 行扱い (line 0 = "a") + line 1 = "" の空末尾行。
#[test]
fn line_index_trailing_newline_creates_empty_last_line() {
    let source: &[u8] = b"a\n";
    let index = LineIndex::new(source);
    // line 0: "a"
    let (s0, e0) = index.line_bounds(source.len(), 0).unwrap();
    assert_eq!(&source[s0..e0], b"a");
    // line 1: 末尾改行直後の空行 (start = 2, end = source.len() = 2)
    let (s1, e1) = index.line_bounds(source.len(), 1).unwrap();
    assert_eq!(s1, 2);
    assert_eq!(e1, 2);
    // line 2: 範囲外
    assert_eq!(index.line_bounds(source.len(), 2), None);
}

/// `LineIndex` で連続改行による空行が正しく検出されることを検証。
/// `b"a\n\nb"` → line 0 "a", line 1 "", line 2 "b"。
#[test]
fn line_index_handles_blank_lines() {
    let source: &[u8] = b"a\n\nb";
    let index = LineIndex::new(source);
    let (s0, e0) = index.line_bounds(source.len(), 0).unwrap();
    assert_eq!(&source[s0..e0], b"a");
    // line 1 は空行 (start = 2, end = 2)
    let (s1, e1) = index.line_bounds(source.len(), 1).unwrap();
    assert_eq!(s1, 2);
    assert_eq!(e1, 2);
    // line 2: 改行なしで終わる最終行
    let (s2, e2) = index.line_bounds(source.len(), 2).unwrap();
    assert_eq!(&source[s2..e2], b"b");
    assert_eq!(index.line_bounds(source.len(), 3), None);
}

/// `LineIndex` が改行なしの単一行を最終行として扱えることを検証。
#[test]
fn line_index_no_trailing_newline_last_line() {
    let source: &[u8] = b"only";
    let index = LineIndex::new(source);
    let (start, end) = index.line_bounds(source.len(), 0).unwrap();
    assert_eq!(&source[start..end], b"only");
    assert_eq!(index.line_bounds(source.len(), 1), None);
}

/// `LineIndex::row_col` が単発走査版 `byte_offset_to_row_col` と全 offset で一致すること。
/// 埋め込み領域の位置計算はこの 2 経路を混在して使うため、ズレると `path:line:col` が壊れる。
#[test]
fn line_index_row_col_matches_single_shot_scan() {
    for text in ["", "a", "a\n", "a\nbb\n\nccc", "\n\n", "あ\nいい"] {
        let index = LineIndex::new(text.as_bytes());
        // 末尾 +2 まで見て、範囲外 offset のクランプ挙動も一致させる。
        for byte in 0..=text.len() + 2 {
            assert_eq!(
                index.row_col(text.len(), byte),
                byte_offset_to_row_col(text, byte),
                "text={text:?} byte={byte}"
            );
        }
    }
}

/// `byte_offset_to_row_col` の基本形: 改行数が row、直前行頭からの byte 数が col。
#[test]
fn byte_offset_to_row_col_counts_rows_and_byte_columns() {
    let text = "ab\ncde\n";
    assert_eq!(byte_offset_to_row_col(text, 0), (0, 0));
    assert_eq!(byte_offset_to_row_col(text, 2), (0, 2));
    // 改行直後は次行の col 0
    assert_eq!(byte_offset_to_row_col(text, 3), (1, 0));
    assert_eq!(byte_offset_to_row_col(text, 5), (1, 2));
    assert_eq!(byte_offset_to_row_col(text, 7), (2, 0));
    // 範囲外は末尾にクランプ
    assert_eq!(byte_offset_to_row_col(text, 999), (2, 0));
    // col は byte 単位 (多バイト文字を 1 と数えない)
    assert_eq!(byte_offset_to_row_col("あa", 3), (0, 3));
}

/// `absolute_position` の規約: 領域 1 行目のみ base 列を加算し、2 行目以降は加算しない。
#[test]
fn absolute_position_adds_base_column_only_on_first_row() {
    assert_eq!(absolute_position((10, 5), (0, 0)), (10, 5));
    assert_eq!(absolute_position((10, 5), (0, 3)), (10, 8));
    // 2 行目以降は行頭がファイルの行頭と一致するため col はそのまま
    assert_eq!(absolute_position((10, 5), (1, 0)), (11, 0));
    assert_eq!(absolute_position((10, 5), (2, 7)), (12, 7));
}

/// `extract_line_context_indexed` が単発 `extract_line_context` と同じ結果を返すことを
/// 検証 (LineIndex 経由でも従来挙動が維持される)。
#[test]
fn extract_line_context_indexed_matches_legacy_path() {
    let source = b"alpha\n  beta  \ngamma";
    let index = LineIndex::new(source);
    for row in 0..4 {
        assert_eq!(
            extract_line_context_indexed(source, &index, row),
            extract_line_context(source, row),
            "row={row}"
        );
    }
}
