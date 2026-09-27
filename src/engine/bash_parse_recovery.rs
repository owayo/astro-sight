//! Bash 文法で解析できなかった宣言の証拠。シンボルとして復元せず未検証のまま保持する。

use tree_sitter::Node;

#[derive(Debug, Clone)]
pub(crate) struct UnparsedBashDeclaration {
    pub(crate) name: String,
    pub(crate) line: usize,
    pub(crate) byte: usize,
}

/// API 差分と refs が同じ宣言ヘッダ判定を使う。正常な木では走査しない。
pub(crate) fn unparsed_declarations(root: Node<'_>, source: &[u8]) -> Vec<UnparsedBashDeclaration> {
    if !root.has_error() {
        return Vec::new();
    }
    let mut declarations = Vec::new();
    let mut opaque_until = 0;
    let mut brace_depth = 0usize;
    let mut cursor = root.walk();
    loop {
        let node = cursor.node();
        // ERROR に平坦化された引用 / 置換 / 算術式 / 条件式 / subshell は、閉じ位置を推測しない。
        // 関数ヘッダの空括弧以外は親 ERROR の終端まで不透明として扱う。
        if let Some(parent) = node.parent().filter(Node::is_error) {
            match node.kind() {
                "<<" | "<<-" | "\"" | "'" | "$'" | "`" | "$(" | "<(" | ">(" | "${" | "$(("
                | "((" | "$[" | "[[" => {
                    opaque_until = opaque_until.max(parent.end_byte());
                }
                "(" if !source[node.end_byte()..]
                    .trim_ascii_start()
                    .starts_with(b")") =>
                {
                    opaque_until = opaque_until.max(parent.end_byte());
                }
                // 関数本体も ERROR に平坦化される。祖先の kind だけでは、閉じ忘れた
                // 本体の中にある同名関数をトップレベルの代替と誤認する。
                "{" => brace_depth += 1,
                "}" => brace_depth = brace_depth.saturating_sub(1),
                _ => {}
            }
        }
        if node.start_byte() >= opaque_until
            && brace_depth == 0
            && let Some(name) = error_node_declaration_name(node, source)
        {
            declarations.push(UnparsedBashDeclaration {
                name: name.to_owned(),
                line: node.start_position().row,
                byte: node.start_byte(),
            });
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return declarations;
            }
        }
    }
}

fn error_node_declaration_name<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    // function キーワードの無いヘッダは、エラー回復時に extglob として残ることがある。
    if !matches!(node.kind(), "word" | "extglob_pattern")
        || !node.parent().is_some_and(|p| p.is_error())
    {
        return None;
    }
    // 文字列・heredoc・コマンド・関数本体の下の ERROR は、トップレベル宣言の証拠にしない。
    let mut ancestor = node.parent();
    while let Some(parent) = ancestor {
        if !parent.is_error() && parent.kind() != "program" {
            return None;
        }
        ancestor = parent.parent();
    }
    let start = node.start_byte();
    let text = node.utf8_text(source).ok()?;
    let name = if node.kind() == "extglob_pattern" {
        text.strip_suffix("()").unwrap_or(text)
    } else {
        text
    };
    // パターンの断片を名前に変換しない。空括弧だけをヘッダとして分離する。
    if name.is_empty()
        || name
            .chars()
            .any(|c| c.is_whitespace() || "(){}[]*?;|&<>\\\"'`".contains(c))
    {
        return None;
    }
    let end = start + name.len();
    let line_start = start.checked_sub(node.start_position().column)?;
    let keyword = match source[line_start..start].trim_ascii() {
        b"function" => true,
        b"" => false,
        _ => return None,
    };
    // 次の 1 行までだけを見る。各宣言で末尾まで UTF-8 検証すると O(N²) になる。
    let tail_end = source[end..]
        .iter()
        .enumerate()
        .filter(|(_, b)| **b == b'\n')
        .nth(1)
        .map_or(source.len(), |(p, _)| end + p);
    let Ok(tail) = std::str::from_utf8(&source[end..tail_end]) else {
        return None;
    };
    let mut lines = tail.lines();
    let mut rest = lines.next().unwrap_or("").trim();
    let parens = if let Some(after_open) = rest.strip_prefix('(') {
        if let Some(after_close) = after_open.trim_start().strip_prefix(')') {
            rest = after_close.trim_start();
            true
        } else {
            false
        }
    } else {
        false
    };
    if !keyword && !parens {
        return None;
    }
    if rest.is_empty() {
        rest = lines.next().unwrap_or("").trim_start();
    }
    (rest.starts_with('{') || rest.starts_with('(')).then_some(name)
}

pub(crate) fn is_zsh_source(path: &str, source: &[u8]) -> bool {
    camino::Utf8Path::new(path)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("zsh"))
        || source
            .strip_prefix(b"#!")
            .and_then(|s| s.split(|&b| b == b'\n').next())
            .and_then(|line| std::str::from_utf8(line).ok())
            .is_some_and(|line| {
                line.split_whitespace()
                    .any(|part| part.rsplit('/').next() == Some("zsh"))
            })
}

pub(crate) fn parse_error_message(path: &str, source: &[u8]) -> &'static str {
    if is_zsh_source(path, source) {
        "zsh parsed with Bash grammar: declarations, references and signature changes inside parse error regions are unverified"
    } else {
        "Bash grammar: declarations, references and signature changes inside parse error regions are unverified"
    }
}
