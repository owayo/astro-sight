//! Python 関数ヘッダの比較。文字列内部の空白とタプルのカンマは契約の一部として残す。

use tree_sitter::Node;

/// 本体を除いたヘッダをトークン単位で正規化する。解析不能なら判定を保留する。
pub(crate) fn normalize_function_header(decl: Node<'_>, source: &[u8]) -> Option<String> {
    if decl.kind() != "function_definition" {
        return None;
    }
    let body = decl.child_by_field_name("body")?;
    normalize_signature_range(decl, source, decl.start_byte()..body.start_byte())
}

/// ヘッダ内の部分範囲にも同じ規則を使い、互換な引数追加の判定と比較を揃える。
pub(crate) fn normalize_signature_range(
    root: Node<'_>,
    source: &[u8],
    range: std::ops::Range<usize>,
) -> Option<String> {
    source.get(range.clone())?;
    let mut pending = vec![root];
    let mut tokens = Vec::new();
    while let Some(node) = pending.pop() {
        if node.start_byte() >= range.end || node.end_byte() <= range.start {
            continue;
        }
        if node.is_error() || node.is_missing() {
            return None;
        }
        if node.kind() == "comment" {
            let comment = node.utf8_text(source).ok()?;
            // 型コメントは静的な契約に関わるため、通常コメントと区別する。
            if comment.strip_prefix('#')?.trim_start().starts_with("type:") {
                tokens.push(comment.to_owned());
            }
            continue;
        }
        if node.kind() == ","
            && node.parent().is_some_and(|parent| {
                matches!(parent.kind(), "parameters" | "argument_list") && {
                    let mut next = node.next_sibling();
                    while next.is_some_and(|n| n.kind() == "comment") {
                        next = next.and_then(|n| n.next_sibling());
                    }
                    next.is_some_and(|n| n.kind() == ")")
                }
            })
        {
            continue;
        }
        // f-string も丸ごと保持する。補間式内の整形は同一視せず安全側に倒す。
        if node.kind() == "string" || node.child_count() == 0 {
            if node.has_error() || node.start_byte() < range.start || node.end_byte() > range.end {
                return None;
            }
            tokens.push(node.utf8_text(source).ok()?.to_owned());
        } else {
            let mut cursor = node.walk();
            pending.extend(
                node.children(&mut cursor)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev(),
            );
        }
    }
    let mut out = String::new();
    let mut previous = "";
    for token in &tokens {
        if !out.is_empty()
            && !matches!(
                token.as_str(),
                "(" | "[" | ")" | "]" | "}" | "," | ":" | "."
            )
            && !matches!(previous, "(" | "[" | "{" | ".")
        {
            out.push(' ');
        }
        out.push_str(token);
        // 後続のトークンが型コメントへ飲み込まれないよう、改行は残す。
        if token.starts_with('#') {
            out.push('\n');
        }
        previous = token;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{engine::parser, language::LangId};

    fn header(source: &str) -> String {
        let tree = parser::parse_source(source.as_bytes(), LangId::Python).unwrap();
        assert!(!tree.root_node().has_error(), "{source}");
        normalize_function_header(tree.root_node().named_child(0).unwrap(), source.as_bytes())
            .unwrap()
    }

    #[test]
    fn python_header_formatting_keeps_literal_and_tuple_semantics() {
        let baseline = header("def run(x: str = 'a  b', y=(1,)) -> str:\n    return x\n");
        assert_eq!(
            baseline,
            header(
                "def run(\n x : str = 'a  b', # note\n y = (1,), # last\n) -> str:\n    return x\n"
            )
        );
        assert_ne!(
            baseline,
            header("def run(x: str = 'a b', y=(1,)) -> str:\n    return x\n")
        );
        assert_ne!(
            baseline,
            header("def run(x: str = 'a  b', y=(1)) -> str:\n    return x\n")
        );
        assert_ne!(
            header("def run(x): # type: (int) -> int\n    return x\n"),
            header("def run(x): # type: (str) -> str\n    return x\n")
        );
    }
}
