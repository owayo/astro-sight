/// 指定行の前後コンテキストをスニペットとして生成する。
pub fn generate_snippet(source: &str, line: usize, context_lines: usize) -> String {
    let total_lines = source.lines().count();
    if total_lines == 0 {
        return String::new();
    }

    let start = line.saturating_sub(context_lines);
    let end = (line + context_lines + 1).min(total_lines);

    let width = format!("{end}").len();

    let mut result = String::new();
    for (i, line_text) in source.lines().skip(start).take(end - start).enumerate() {
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
    let view_end = (end_line + context_lines + 1).min(total_lines);
    let width = format!("{view_end}").len();

    let mut result = String::new();
    for (i, line) in source
        .lines()
        .skip(view_start)
        .take(view_end - view_start)
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
