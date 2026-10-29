//! 既知の Rust 書式マクロのリテラルから、暗黙に参照する名前と元の byte 位置を取る。
//!
//! マクロの展開や別名解決は行わない。名前付き引数はキャプチャより優先し、
//! 通常文字列・生成した書式文字列・エスケープした波括弧を参照にしない。

use std::collections::HashSet;
use std::iter::Peekable;

use tree_sitter::Node;

use crate::engine::parser;
use crate::engine::tree_walk::{Children, Walk, walk_preorder};
use crate::language::LangId;

pub(crate) fn has_format_marker(source: &[u8]) -> bool {
    [b"format".as_slice(), b"print", b"write"]
        .iter()
        .any(|word| memchr::memmem::find(source, word).is_some())
}

pub(crate) fn has_escape_marker(source: &[u8]) -> bool {
    memchr::memmem::find(source, b"\\u{").is_some()
        || memchr::memmem::find(source, b"\\x").is_some()
}

pub(crate) fn may_have_escaped_capture(source: &[u8]) -> bool {
    has_escape_marker(source) && has_format_marker(source)
}

fn previous(mut node: Node<'_>) -> Option<Node<'_>> {
    loop {
        node = node.prev_sibling()?;
        if !matches!(node.kind(), "line_comment" | "block_comment") {
            return Some(node);
        }
    }
}

fn macro_name<'a>(tree: Node<'_>, parent: Option<Node<'_>>, source: &'a [u8]) -> Option<&'a str> {
    let mut names = Vec::new();
    if let Some(parent) = parent.filter(|p| p.kind() == "macro_invocation") {
        let name = parent.child_by_field_name("macro")?;
        let _: std::ops::ControlFlow<()> = walk_preorder(name, Children::Named, |part, _| {
            match part.kind() {
                "identifier" => names.push(part.utf8_text(source).ok()),
                "scoped_identifier" | "line_comment" | "block_comment" => {}
                _ => names.push(None),
            }
            std::ops::ControlFlow::Continue(Walk::Descend)
        });
    } else {
        let bang = previous(tree)?;
        if bang.kind() != "!" {
            return None;
        }
        let mut name = previous(bang)?;
        if name.kind() != "identifier" {
            return None;
        }
        names.push(name.utf8_text(source).ok());
        while let Some(separator) = previous(name).filter(|n| n.kind() == "::") {
            let Some(prefix) = previous(separator) else {
                break;
            };
            if matches!(prefix.kind(), "(" | "[" | "{") {
                break;
            }
            if prefix.kind() != "identifier" {
                return None;
            }
            names.push(prefix.utf8_text(source).ok());
            name = prefix;
        }
        names.reverse();
    }
    let names: Vec<_> = names
        .into_iter()
        .collect::<Option<Vec<_>>>()?
        .into_iter()
        .map(|name| name.strip_prefix("r#").unwrap_or(name))
        .collect();
    match names.as_slice() {
        [name] => Some(*name),
        ["std" | "core" | "alloc", name] => Some(*name),
        _ => None,
    }
}

fn format_argument(name: &str) -> Option<usize> {
    match name {
        "format" | "format_args" | "print" | "println" | "eprint" | "eprintln" => Some(0),
        "write" | "writeln" => Some(1),
        _ => None,
    }
}

/// 追加の木はマクロ1個の引数だけ。式内の generic のカンマや入れ子を文字列で分割しない。
pub(crate) fn captures(
    tree: Node<'_>,
    ancestors: &[Node<'_>],
    source: &[u8],
) -> Vec<(String, usize)> {
    if tree.kind() != "token_tree" {
        return Vec::new();
    }
    let Some(index) = macro_name(tree, ancestors.last().copied(), source).and_then(format_argument)
    else {
        return Vec::new();
    };
    for (i, &ancestor) in ancestors.iter().enumerate() {
        if ancestor.kind() == "macro_definition" {
            return Vec::new();
        }
        if ancestor.kind() != "token_tree" {
            continue;
        }
        let parent = ancestors[..i].last().copied();
        let is_macro = parent.is_some_and(|n| n.kind() == "macro_invocation")
            || previous(ancestor).is_some_and(|n| n.kind() == "!");
        if is_macro
            && !macro_name(ancestor, parent, source).is_some_and(|name| {
                format_argument(name).is_some()
                    || matches!(
                        name,
                        "assert"
                            | "assert_eq"
                            | "assert_ne"
                            | "debug_assert"
                            | "debug_assert_eq"
                            | "debug_assert_ne"
                            | "dbg"
                            | "vec"
                            | "matches"
                    )
            })
        {
            return Vec::new();
        }
    }
    // キャプチャを含まない通常の書式では追加の式解析をしない。リテラルの走査は1回だけ。
    let mut cursor = tree.walk();
    let candidates: Vec<_> = tree
        .named_children(&mut cursor)
        .filter_map(|literal| {
            if !matches!(literal.kind(), "string_literal" | "raw_string_literal") {
                return None;
            }
            let text = literal.utf8_text(source).ok()?;
            let (body, raw, prefix) = literal_body(text)?;
            let chars = LiteralChars {
                body,
                offset: 0,
                absolute: literal.start_byte() + prefix,
                raw,
            }
            .peekable();
            let found = format_captures(chars, source, &HashSet::new())?;
            (!found.is_empty()).then_some((literal.start_byte(), found))
        })
        .collect();
    if candidates.is_empty() {
        return Vec::new();
    }
    const PREFIX: &str = "fn __capture(){__args(";
    let mut snippet = PREFIX.as_bytes().to_vec();
    let mut positions = Vec::new();
    let mut end = tree.start_byte() + 1;
    let mut cursor = tree.walk();
    for child in tree.children(&mut cursor) {
        if child.start_byte() < end || child.end_byte() >= tree.end_byte() {
            continue;
        }
        snippet.extend_from_slice(&source[end..child.start_byte()]);
        positions.push((snippet.len(), child.start_byte()));
        if child.kind() == "token_tree" {
            // 入れ子の式を再解析せず、同じ区切りの単純な式へ畳む。byte位置は別に対応付ける。
            snippet.extend_from_slice(match source[child.start_byte()] {
                b'(' => b"(__capture)",
                b'[' => b"[__capture]",
                _ => b"{}",
            });
        } else {
            snippet.extend_from_slice(&source[child.byte_range()]);
        }
        end = child.end_byte();
    }
    snippet.extend_from_slice(&source[end..tree.end_byte() - 1]);
    snippet.extend_from_slice(b");}");
    let Ok(parsed) = parser::parse_source(&snippet, LangId::Rust) else {
        return Vec::new();
    };
    let root = parsed.root_node();
    if root.has_error() {
        return Vec::new();
    }
    let Some(arguments) = root
        .named_child(0)
        .and_then(|n| n.child_by_field_name("body"))
        .and_then(|n| n.named_child(0))
        .and_then(|n| n.named_child(0))
        .and_then(|n| n.child_by_field_name("arguments"))
    else {
        return Vec::new();
    };
    let mut cursor = arguments.walk();
    let args: Vec<_> = arguments
        .named_children(&mut cursor)
        .filter(|n| !matches!(n.kind(), "line_comment" | "block_comment"))
        .collect();
    let Some(literal) = args.get(index).copied() else {
        return Vec::new();
    };
    if !matches!(literal.kind(), "string_literal" | "raw_string_literal") {
        return Vec::new();
    }
    let explicit: HashSet<_> = args
        .iter()
        .skip(index + 1)
        .filter_map(|arg| {
            if arg.kind() != "assignment_expression" {
                return None;
            }
            let left = arg.child_by_field_name("left")?;
            (left.kind() == "identifier")
                .then(|| left.utf8_text(&snippet).ok())
                .flatten()
                .map(|name| name.strip_prefix("r#").unwrap_or(name))
        })
        .collect();
    let Some((_, absolute)) = positions
        .into_iter()
        .find(|(start, _)| *start == literal.start_byte())
    else {
        return Vec::new();
    };
    let Some((_, mut found)) = candidates.into_iter().find(|(start, _)| *start == absolute) else {
        return Vec::new();
    };
    found.retain(|(name, _)| !explicit.contains(name.as_str()));
    found
}

fn literal_body(text: &str) -> Option<(&str, bool, usize)> {
    let quote = text.find('"')?;
    let raw = text.starts_with('r');
    if quote != 0 && !raw {
        return None;
    }
    let suffix = if raw { quote } else { 1 };
    Some((
        text.get(quote + 1..text.len().checked_sub(suffix)?)?,
        raw,
        quote + 1,
    ))
}

#[derive(Clone)]
struct LiteralChars<'a> {
    body: &'a str,
    offset: usize,
    absolute: usize,
    raw: bool,
}

impl LiteralChars<'_> {
    fn take(&mut self) -> Option<char> {
        let ch = self.body.get(self.offset..)?.chars().next()?;
        self.offset += ch.len_utf8();
        Some(ch)
    }
}

impl Iterator for LiteralChars<'_> {
    type Item = (char, usize);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let at = self.offset;
            let ch = self.take()?;
            if ch == '\r' && self.body.as_bytes().get(self.offset) == Some(&b'\n') {
                continue;
            }
            if self.raw || ch != '\\' {
                return Some((ch, self.absolute + at));
            }
            let escaped = match self.take()? {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                '0' => '\0',
                '\\' => '\\',
                '\'' => '\'',
                '"' => '"',
                '\n' | '\r' => {
                    while self
                        .body
                        .get(self.offset..)?
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_whitespace())
                    {
                        self.take();
                    }
                    continue;
                }
                'x' => {
                    let value = self.take()?.to_digit(16)? * 16 + self.take()?.to_digit(16)?;
                    if value > 0x7f {
                        return None;
                    }
                    char::from_u32(value)?
                }
                'u' => {
                    if self.take()? != '{' {
                        return None;
                    }
                    let mut value = 0u32;
                    loop {
                        match self.take()? {
                            '}' => break,
                            '_' => {}
                            c => value = value.checked_mul(16)?.checked_add(c.to_digit(16)?)?,
                        }
                    }
                    char::from_u32(value)?
                }
                _ => return None,
            };
            return Some((escaped, self.absolute + at));
        }
    }
}

type Chars<'a> = Peekable<LiteralChars<'a>>;

fn word(chars: &mut Chars<'_>) -> (String, usize) {
    let at = chars.peek().map_or(0, |p| p.1);
    let mut name = String::new();
    while chars.peek().is_some_and(|(c, _)| {
        c.is_ascii_alphanumeric() || *c == '_' || (!c.is_ascii() && !c.is_whitespace())
    }) {
        name.push(chars.next().unwrap().0);
    }
    (name, at)
}

fn valid_name(name: &str) -> bool {
    if name.is_empty() || name == "_" || name.as_bytes()[0].is_ascii_digit() {
        return false;
    }
    if name.is_ascii() {
        return true;
    }
    // 非ASCII名は手作りのUnicode表で近似せず、実際のRust文法で識別子を検証する。
    let probe = format!("const r#{name}: () = ();");
    parser::parse_source(probe.as_bytes(), LangId::Rust)
        .is_ok_and(|tree| !tree.root_node().has_error())
}

fn push_capture(
    name: String,
    at: usize,
    _source: &[u8],
    explicit: &HashSet<&str>,
    out: &mut Vec<(String, usize)>,
) {
    if valid_name(&name) && !explicit.contains(name.as_str()) {
        out.push((name, at));
    }
}

fn count(
    chars: &mut Chars<'_>,
    source: &[u8],
    explicit: &HashSet<&str>,
    out: &mut Vec<(String, usize)>,
) {
    let (name, at) = word(chars);
    if chars.next_if(|p| p.0 == '$').is_some() {
        push_capture(name, at, source, explicit, out);
    }
}

fn format_captures(
    mut chars: Chars<'_>,
    source: &[u8],
    explicit: &HashSet<&str>,
) -> Option<Vec<(String, usize)>> {
    let mut out = Vec::new();
    while let Some((ch, _)) = chars.next() {
        match ch {
            '{' => {
                if chars.next_if(|p| p.0 == '{').is_some() {
                    continue;
                }
                let (name, at) = word(&mut chars);
                push_capture(name, at, source, explicit, &mut out);
                if chars.next_if(|p| p.0 == ':').is_some() {
                    let mut look = chars.clone();
                    look.next();
                    if look.peek().is_some_and(|p| matches!(p.0, '<' | '^' | '>')) {
                        chars.next();
                        chars.next();
                    } else {
                        chars.next_if(|p| matches!(p.0, '<' | '^' | '>'));
                    }
                    chars.next_if(|p| matches!(p.0, '+' | '-'));
                    chars.next_if(|p| p.0 == '#');
                    chars.next_if(|p| p.0 == '0');
                    count(&mut chars, source, explicit, &mut out);
                    if chars.next_if(|p| p.0 == '.').is_some()
                        && chars.next_if(|p| p.0 == '*').is_none()
                    {
                        count(&mut chars, source, explicit, &mut out);
                    }
                    while chars.peek().is_some_and(|p| p.0 != '}') {
                        if chars.next()?.0 == '{' {
                            return None;
                        }
                    }
                }
                while chars.peek().is_some_and(|p| p.0.is_whitespace()) {
                    chars.next();
                }
                if chars.next()?.0 != '}' {
                    return None;
                }
            }
            '}' => {
                chars.next_if(|p| p.0 == '}')?;
            }
            _ => {}
        }
    }
    Some(out)
}
