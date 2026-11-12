//! C として読む共用ヘッダーの C++ 用 linkage 包装を、前処理行の対応から確定する。
//! コメントは判定用の写しでだけ空白にし、最終解析には元のコメントと識別子を残す。

use std::borrow::Cow;
use std::ops::{ControlFlow, Range as ByteRange};

use tree_sitter::{Point, Range, Tree};

use crate::engine::tree_walk::{Children, Walk, walk_preorder};

#[derive(Clone, Copy)]
struct Line<'a> {
    start: usize,
    text: &'a [u8],
}

pub(super) fn included_ranges(tree: &Tree, source: &[u8]) -> Option<Vec<Range>> {
    let mut view = Cow::Borrowed(source);
    let _: ControlFlow<()> = walk_preorder(tree.root_node(), Children::Named, |node, _| {
        if node.kind() == "comment" {
            for byte in &mut view.to_mut()[node.byte_range()] {
                if !matches!(*byte, b'\r' | b'\n') {
                    *byte = b' ';
                }
            }
            return ControlFlow::Continue(Walk::Skip);
        }
        ControlFlow::Continue(Walk::Descend)
    });
    let mut lines = view
        .split_inclusive(|byte| *byte == b'\n')
        .scan(0, |offset, line| {
            let leading = line.trim_ascii_start();
            let start = *offset + line.len() - leading.len();
            *offset += line.len();
            Some(Line {
                start,
                text: leading.trim_ascii_end(),
            })
        })
        .filter(|line| !line.text.is_empty());
    let mut contexts = Vec::new();
    let mut context = 0usize;
    let mut next_context = 1usize;
    let mut pending = Vec::new();
    let mut gaps = Vec::new();
    while let Some(line) = lines.next() {
        if cpp_guard(line.text) {
            let mut ahead = lines.clone();
            if let Some(body) = ahead.next() {
                let opening = opening(body, &mut ahead);
                let closing = body.text == b"}";
                if opening.is_some() || closing {
                    if !ahead
                        .next()
                        .is_some_and(|line| directive(line.text) == Some((&b"endif"[..], &b""[..])))
                    {
                        return None;
                    }
                    if let Some(opening) = opening {
                        pending.push((context, opening));
                    } else {
                        let (owner, opening) = pending.pop()?;
                        if owner != context {
                            return None;
                        }
                        gaps.push(opening);
                        gaps.push(body.start..body.start + 1);
                    }
                    lines = ahead;
                    continue;
                }
            }
        }
        if let Some((keyword, rest)) = directive(line.text) {
            match keyword {
                b"if" | b"ifdef" | b"ifndef" => {
                    contexts.push(context);
                    context = next_context;
                    next_context += 1;
                }
                b"else" | b"elif" | b"elifdef" | b"elifndef" => {
                    contexts.last()?;
                    context = next_context;
                    next_context += 1;
                }
                b"endif" if rest.is_empty() => context = contexts.pop()?,
                b"endif" => return None,
                _ => {}
            }
        }
    }
    if !pending.is_empty() || !contexts.is_empty() || gaps.is_empty() {
        return None;
    }
    ranges_around(gaps, source)
}

fn opening<'a>(
    line: Line<'a>,
    lines: &mut impl Iterator<Item = Line<'a>>,
) -> Option<ByteRange<usize>> {
    let rest = line
        .text
        .strip_prefix(b"extern")?
        .trim_ascii_start()
        .strip_prefix(b"\"C\"")?
        .trim_ascii();
    let end = if rest == b"{" {
        line.start + line.text.len()
    } else if rest.is_empty() {
        let brace = lines.next()?;
        if brace.text != b"{" {
            return None;
        }
        brace.start + 1
    } else {
        return None;
    };
    Some(line.start..end)
}

fn directive(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let rest = line.strip_prefix(b"#")?.trim_ascii_start();
    let length = rest
        .iter()
        .take_while(|byte| byte.is_ascii_alphanumeric() || **byte == b'_')
        .count();
    (length > 0).then(|| (&rest[..length], rest[length..].trim_ascii()))
}

fn cpp_guard(line: &[u8]) -> bool {
    let Some((keyword, condition)) = directive(line) else {
        return false;
    };
    match keyword {
        b"ifdef" => condition == b"__cplusplus",
        b"if" => {
            if condition == b"__cplusplus" {
                return true;
            }
            let Some(rest) = condition.strip_prefix(b"defined") else {
                return false;
            };
            if !rest
                .first()
                .is_some_and(|byte| byte.is_ascii_whitespace() || *byte == b'(')
            {
                return false;
            }
            let rest = rest.trim_ascii();
            rest.strip_prefix(b"(")
                .and_then(|rest| rest.strip_suffix(b")"))
                .unwrap_or(rest)
                .trim_ascii()
                == b"__cplusplus"
        }
        _ => false,
    }
}

fn ranges_around(mut gaps: Vec<ByteRange<usize>>, source: &[u8]) -> Option<Vec<Range>> {
    gaps.sort_by_key(|range| (range.start, range.end));
    let mut included = Vec::with_capacity(gaps.len() + 1);
    let mut start_byte = 0;
    let mut start_point = Point::new(0, 0);
    let mut newlines = memchr::memchr_iter(b'\n', source).peekable();
    let mut row = 0;
    let mut line_start = 0;
    let mut point = |byte| {
        while newlines.peek().is_some_and(|newline| *newline < byte) {
            line_start = newlines.next().unwrap() + 1;
            row += 1;
        }
        Point::new(row, byte - line_start)
    };
    for gap in gaps {
        if gap.start < start_byte {
            return None;
        }
        let gap_start = point(gap.start);
        let gap_end = point(gap.end);
        if start_byte < gap.start {
            included.push(Range {
                start_byte,
                start_point,
                end_byte: gap.start,
                end_point: gap_start,
            });
        }
        start_byte = gap.end;
        start_point = gap_end;
    }
    if start_byte < source.len() {
        included.push(Range {
            start_byte,
            start_point,
            end_byte: source.len(),
            end_point: point(source.len()),
        });
    }
    Some(included)
}
