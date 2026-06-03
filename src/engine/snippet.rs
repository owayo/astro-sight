/// 指定行の前後コンテキストをスニペットとして生成する。
pub fn generate_snippet(source: &str, line: usize, context_lines: usize) -> String {
    let total_lines = source.lines().count();
    if total_lines == 0 {
        return String::new();
    }

    let start = line.saturating_sub(context_lines);
    // line は CLI 入力由来で usize::MAX もあり得るため、加算も saturating で overflow を防ぐ。
    let end = line
        .saturating_add(context_lines)
        .saturating_add(1)
        .min(total_lines);

    let width = format!("{end}").len();

    let mut result = String::new();
    for (i, line_text) in source
        .lines()
        .skip(start)
        .take(end.saturating_sub(start))
        .enumerate()
    {
        let line_num = start + i;
        let marker = if line_num == line { ">" } else { " " };
        let display = truncate_snippet_line(line_text);
        result.push_str(&format!("{marker}{line_num:>width$} | {display}\n"));
    }
    result
}

/// 指定範囲の前後コンテキストをスニペットとして生成する。
pub fn generate_range_snippet(
    source: &str,
    start_line: usize,
    end_line: usize,
    context_lines: usize,
) -> String {
    let total_lines = source.lines().count();
    if total_lines == 0 {
        return String::new();
    }

    let view_start = start_line.saturating_sub(context_lines);
    // end_line も CLI 入力由来のため加算を saturating にして overflow を防ぐ。
    let view_end = end_line
        .saturating_add(context_lines)
        .saturating_add(1)
        .min(total_lines);
    let width = format!("{view_end}").len();

    let mut result = String::new();
    for (i, line) in source
        .lines()
        .skip(view_start)
        .take(view_end.saturating_sub(view_start))
        .enumerate()
    {
        let line_num = view_start + i;
        let marker = if line_num >= start_line && line_num <= end_line {
            ">"
        } else {
            " "
        };
        let display = truncate_snippet_line(line);
        result.push_str(&format!("{marker}{line_num:>width$} | {display}\n"));
    }
    result
}

const MAX_SNIPPET_LINE_LEN: usize = 256;

fn truncate_snippet_line(line: &str) -> String {
    if line.len() <= MAX_SNIPPET_LINE_LEN {
        line.to_string()
    } else {
        // 巨大行でも UTF-8 境界を壊さずに切り詰める。
        let truncated = &line[..line.floor_char_boundary(MAX_SNIPPET_LINE_LEN)];
        format!("{truncated}...")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippet_basic() {
        let source = "line0\nline1\nline2\nline3\nline4\n";
        let snippet = generate_snippet(source, 2, 1);
        assert!(snippet.contains("line1"));
        assert!(snippet.contains("line2"));
        assert!(snippet.contains("line3"));
        assert!(snippet.contains(">"));
    }

    /// line がファイル行数を超えても usize 減算アンダーフローでパニックしない
    /// (修正前は debug ビルドで `end - start` がパニックしていた)
    #[test]
    fn snippet_line_beyond_eof_no_underflow() {
        let source = "only\ntwo\n";
        let snippet = generate_snippet(source, 999_999, 2);
        assert!(snippet.is_empty());
    }

    /// range スニペットも範囲超過で `view_end - view_start` のアンダーフローを起こさない
    #[test]
    fn range_snippet_beyond_eof_no_underflow() {
        let source = "only\ntwo\n";
        let snippet = generate_range_snippet(source, 999_999, 1_000_000, 2);
        assert!(snippet.is_empty());
    }

    /// `--line` / `--end-line` が usize::MAX でも `line + context + 1` の加算 overflow を
    /// 起こさない (codex 指摘: 減算だけでなく加算側にも上限が必要)。
    #[test]
    fn snippet_usize_max_no_add_overflow() {
        let source = "only\ntwo\n";
        let s = generate_snippet(source, usize::MAX, 1);
        assert!(s.is_empty());
        // range 版: end_line=usize::MAX でも panic せず view_start..total_lines を出力する。
        let r = generate_range_snippet(source, 1, usize::MAX, 1);
        assert!(r.contains("only"));
    }

    #[test]
    fn snippet_start_of_file() {
        let source = "first\nsecond\nthird\n";
        let snippet = generate_snippet(source, 0, 2);
        assert!(snippet.contains("first"));
    }

    #[test]
    fn snippet_truncates_long_utf8_line() {
        let source = format!("{}\nsecond\n", "あ".repeat(300));
        let snippet = generate_snippet(&source, 0, 0);
        let display = snippet
            .lines()
            .next()
            .and_then(|line| line.split("| ").nth(1))
            .expect("snippet line");

        assert!(display.ends_with("..."));
        assert!(display.len() <= MAX_SNIPPET_LINE_LEN + 3);
    }

    #[test]
    fn snippet_preserves_indentation() {
        let source = "    indented\n";
        let snippet = generate_snippet(source, 0, 0);
        let display = snippet
            .lines()
            .next()
            .and_then(|line| line.split("| ").nth(1))
            .expect("snippet line");

        assert_eq!(display, "    indented");
    }

    #[test]
    fn range_snippet_marks_selected_lines() {
        let source = "zero\none\ntwo\nthree\n";
        let snippet = generate_range_snippet(source, 1, 2, 0);
        let lines: Vec<&str> = snippet.lines().collect();

        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with(">"));
        assert!(lines[1].starts_with(">"));
    }
}
