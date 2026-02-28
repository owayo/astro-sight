/// Generate a context snippet around a given line.
pub fn generate_snippet(source: &str, line: usize, context_lines: usize) -> String {
    let lines: Vec<&str> = source.lines().collect();
    if lines.is_empty() {
        return String::new();
    }

    let start = line.saturating_sub(context_lines);
    let end = (line + context_lines + 1).min(lines.len());

    let width = format!("{end}").len();

    let mut result = String::new();
    for (i, l) in lines[start..end].iter().enumerate() {
        let line_num = start + i;
        let marker = if line_num == line { ">" } else { " " };
        result.push_str(&format!("{marker}{line_num:>width$} | {l}\n"));
    }
    result
}

/// Generate a range snippet.
pub fn generate_range_snippet(
    source: &str,
    start_line: usize,
    end_line: usize,
    context_lines: usize,
) -> String {
    let lines: Vec<&str> = source.lines().collect();
    if lines.is_empty() {
        return String::new();
    }

    let view_start = start_line.saturating_sub(context_lines);
    let view_end = (end_line + context_lines + 1).min(lines.len());
    let width = format!("{view_end}").len();

    let mut result = String::new();
    for (i, l) in lines[view_start..view_end].iter().enumerate() {
        let line_num = view_start + i;
        let marker = if line_num >= start_line && line_num <= end_line {
            ">"
        } else {
            " "
        };
        result.push_str(&format!("{marker}{line_num:>width$} | {l}\n"));
    }
    result
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
}
