//! local macro の限定展開で、束縛を変えない private メソッド本体だけを識別する。

use super::tokens_of;
use crate::engine::parser;
use crate::engine::tree_walk::{Children, Walk, any_node, walk_preorder};
use crate::language::LangId;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::{ControlFlow, Range};
use tree_sitter::Node;

/// 証明用の展開だけを制限する。超えた呼び出しも従来どおり opaque として解析する。
const MAX_PROOF_BYTES: usize = 1_000_000;

/// 元の source/tree は変えず、束縛比較用のトークンからだけ証明済み本体を除く。
pub(super) fn binding_tokens(
    node: Node<'_>,
    source: &[u8],
    bodies: &[Range<usize>],
) -> Vec<String> {
    let at = bodies.partition_point(|body| body.end <= node.start_byte());
    if bodies
        .get(at)
        .is_none_or(|body| body.start >= node.end_byte())
    {
        return tokens_of(node, source);
    }
    let mut tokens = Vec::new();
    let _ = walk_preorder(node, Children::All, |node, _| {
        let at = bodies.partition_point(|body| body.end <= node.start_byte());
        match bodies.get(at) {
            Some(body) if body.start <= node.start_byte() && node.end_byte() <= body.end => {
                ControlFlow::<(), Walk>::Continue(Walk::Skip)
            }
            Some(body) if body.start < node.end_byte() => ControlFlow::Continue(Walk::Descend),
            _ => {
                super::push_tokens(node, source, &mut tokens);
                ControlFlow::Continue(Walk::Skip)
            }
        }
    });
    tokens
}

/// 同じファイル直下の、無属性・単一 arm の定義と呼び出しだけを証明する。
pub(super) fn ranges(root: Node<'_>, source: &[u8]) -> Vec<Range<usize>> {
    if source.len() > MAX_PROOF_BYTES {
        return Vec::new();
    }
    let mut cursor = root.walk();
    let items: Vec<_> = root.named_children(&mut cursor).collect();
    if items
        .iter()
        .any(|item| matches!(item.kind(), "attribute_item" | "inner_attribute_item"))
    {
        return Vec::new();
    }
    let mut definitions = BTreeMap::<&str, Vec<Node<'_>>>::new();
    let mut invocations = Vec::new();
    for item in items {
        if item.kind() == "macro_definition"
            && let Some(name) = item
                .child_by_field_name("name")
                .and_then(|name| name.utf8_text(source).ok())
        {
            definitions.entry(name).or_default().push(item);
        }
        let invocation = match item.kind() {
            "macro_invocation" => Some(item),
            "expression_statement" => {
                let mut cursor = item.walk();
                item.named_children(&mut cursor)
                    .find(|child| child.kind() == "macro_invocation")
            }
            _ => None,
        };
        invocations.extend(invocation);
    }
    // 先行する未知の展開は、textual scope に別の定義を持ち込める。
    let [invocation] = invocations.as_slice() else {
        return Vec::new();
    };
    let Some(name) = invocation
        .child_by_field_name("macro")
        .filter(|name| name.kind() == "identifier")
    else {
        return Vec::new();
    };
    let Some([definition]) = name
        .utf8_text(source)
        .ok()
        .and_then(|name| definitions.get(name))
        .map(Vec::as_slice)
    else {
        return Vec::new();
    };
    if definition.end_byte() > invocation.start_byte() {
        return Vec::new();
    }
    // 子 module の内側の macro_use も親の textual scope を変え得る。
    let mut cursor = root.walk();
    if root
        .named_children(&mut cursor)
        .any(|item| item.kind() == "mod_item" && item.start_byte() < invocation.start_byte())
    {
        return Vec::new();
    }
    prove(*definition, *invocation, source).unwrap_or_default()
}

#[derive(Clone)]
struct Token<'a> {
    text: &'a str,
    origin: Option<Range<usize>>,
    identifier: bool,
}

fn tokens<'a>(node: Node<'_>, source: &'a [u8], argument: bool) -> Option<Vec<Token<'a>>> {
    if node.byte_range().len() > MAX_PROOF_BYTES {
        return None;
    }
    let mut tokens = Vec::new();
    walk_preorder(node, Children::All, |node, _| {
        if matches!(node.kind(), "line_comment" | "block_comment") {
            // doc コメントは属性にもなるので、証明ではコメントを一律に受け付けない。
            return ControlFlow::Break(());
        }
        if node.child_count() == 0
            || matches!(
                node.kind(),
                "metavariable"
                    | "lifetime"
                    | "string_literal"
                    | "raw_string_literal"
                    | "char_literal"
            )
        {
            let Ok(text) = node.utf8_text(source) else {
                return ControlFlow::Break(());
            };
            tokens.push(Token {
                text,
                origin: argument.then(|| node.byte_range()),
                identifier: node.kind() == "identifier",
            });
            ControlFlow::Continue(Walk::Skip)
        } else {
            ControlFlow::Continue(Walk::Descend)
        }
    })
    .is_continue()
    .then_some(tokens)
}

fn interior<'a>(tokens: &'a [Token<'a>]) -> Option<&'a [Token<'a>]> {
    match tokens {
        [first, inner @ .., last]
            if matches!(
                (first.text, last.text),
                ("(", ")") | ("[", "]") | ("{", "}")
            ) =>
        {
            Some(inner)
        }
        _ => None,
    }
}

/// ident の捕捉と、末尾の `$($tokens:tt)*` だけを読む。未知の先行 arm を飛ばさない。
fn captures<'a>(
    pattern: &[Token<'a>],
    arguments: &[Token<'a>],
) -> Option<BTreeMap<&'a str, Captured<'a>>> {
    let mut captures = BTreeMap::new();
    let mut at = 0;
    let mut input = 0;
    while at < pattern.len() {
        let rest = &pattern[at..];
        if let [dollar, open, name, colon, fragment, close, star] = rest
            && [
                dollar.text,
                open.text,
                colon.text,
                fragment.text,
                close.text,
                star.text,
            ] == ["$", "(", ":", "tt", ")", "*"]
            && name.text.starts_with('$')
            && name.text != "$crate"
        {
            captures
                .insert(
                    name.text,
                    Captured {
                        tokens: arguments[input..].to_vec(),
                        repeated: true,
                    },
                )
                .is_none()
                .then_some(())?;
            return Some(captures);
        }
        if let [name, colon, fragment, ..] = rest
            && name.text.starts_with('$')
            && name.text != "$crate"
            && colon.text == ":"
            && fragment.text == "ident"
        {
            let argument = arguments.get(input)?.clone();
            if !argument.identifier {
                return None;
            }
            captures
                .insert(
                    name.text,
                    Captured {
                        tokens: vec![argument],
                        repeated: false,
                    },
                )
                .is_none()
                .then_some(())?;
            at += 3;
        } else {
            if rest[0].text.starts_with('$') || rest[0].text != arguments.get(input)?.text {
                return None;
            }
            at += 1;
        }
        input += 1;
    }
    (input == arguments.len()).then_some(captures)
}

struct Captured<'a> {
    tokens: Vec<Token<'a>>,
    repeated: bool,
}

fn expand<'a>(
    template: &[Token<'a>],
    captures: &BTreeMap<&str, Captured<'a>>,
) -> Option<Vec<Token<'a>>> {
    let mut expanded = Vec::new();
    let mut size = 0usize;
    let mut at = 0;
    while at < template.len() {
        let token = &template[at];
        let added = if token.text == "$" {
            let [_, open, name, close, star, ..] = &template[at..] else {
                return None;
            };
            if [open.text, close.text, star.text] != ["(", ")", "*"] {
                return None;
            }
            at += 5;
            let captured = captures.get(name.text)?;
            if !captured.repeated {
                return None;
            }
            captured.tokens.as_slice()
        } else if token.text.starts_with('$') {
            at += 1;
            let captured = captures.get(token.text)?;
            if captured.repeated || captured.tokens.len() != 1 {
                return None;
            }
            captured.tokens.as_slice()
        } else {
            at += 1;
            std::slice::from_ref(token)
        };
        for token in added {
            size = size.checked_add(token.text.len() + 1)?;
            if size > MAX_PROOF_BYTES {
                return None;
            }
            expanded.push(token.clone());
        }
    }
    Some(expanded)
}

fn prove(definition: Node<'_>, invocation: Node<'_>, source: &[u8]) -> Option<Vec<Range<usize>>> {
    if definition
        .byte_range()
        .len()
        .checked_add(invocation.byte_range().len())?
        > MAX_PROOF_BYTES
    {
        return None;
    }
    let mut cursor = definition.walk();
    let rules: Vec<_> = definition
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "macro_rule")
        .collect();
    let [rule] = rules.as_slice() else {
        return None;
    };
    let pattern = tokens(rule.child_by_field_name("left")?, source, false)?;
    let template = tokens(rule.child_by_field_name("right")?, source, false)?;
    let mut cursor = invocation.walk();
    let arguments = tokens(
        invocation
            .named_children(&mut cursor)
            .find(|child| child.kind() == "token_tree")?,
        source,
        true,
    )?;
    let captures = captures(interior(&pattern)?, interior(&arguments)?)?;
    let expanded = expand(interior(&template)?, &captures)?;
    let mut text = String::new();
    let mut mapped = Vec::new();
    for token in &expanded {
        let start = text.len();
        text.push_str(token.text);
        mapped.push((start..text.len(), token.origin.clone()));
        text.push(' ');
    }
    let tree = parser::parse_source(text.as_bytes(), LangId::Rust).ok()?;
    let bodies = private_bodies(tree.root_node(), text.as_bytes())?;
    let mut hidden = BTreeSet::new();
    let mut visible = BTreeSet::new();
    for (span, origin) in mapped {
        let Some(origin) = origin else {
            continue;
        };
        let key = (origin.start, origin.end);
        let at = bodies.partition_point(|body| body.end <= span.start);
        if bodies
            .get(at)
            .is_some_and(|body| body.start <= span.start && span.end <= body.end)
        {
            hidden.insert(key);
        } else {
            visible.insert(key);
        }
    }
    // 同じ捕捉を本体の外にも転記する規則なら、その入力は消せない。
    if !hidden.is_disjoint(&visible) {
        return None;
    }
    Some(hidden.into_iter().map(|(start, end)| start..end).collect())
}

fn private_bodies(root: Node<'_>, source: &[u8]) -> Option<Vec<Range<usize>>> {
    if root.has_error()
        || any_node(root, Children::Named, |node| {
            matches!(
                node.kind(),
                "attribute_item"
                    | "inner_attribute_item"
                    | "macro_invocation"
                    | "macro_definition"
                    | "visibility_modifier"
            )
        })
    {
        return None;
    }
    let mut cursor = root.walk();
    let items: Vec<_> = root.named_children(&mut cursor).collect();
    let [structure, implementation] = items.as_slice() else {
        return None;
    };
    if structure.kind() != "struct_item"
        || structure.child_by_field_name("body").is_some()
        || structure.child_by_field_name("type_parameters").is_some()
        || implementation.kind() != "impl_item"
        || implementation.child_by_field_name("trait").is_some()
        || implementation
            .child_by_field_name("type_parameters")
            .is_some()
    {
        return None;
    }
    let name = structure
        .child_by_field_name("name")?
        .utf8_text(source)
        .ok()?;
    let owner = implementation.child_by_field_name("type")?;
    if owner.kind() != "type_identifier" || owner.utf8_text(source).ok()? != name {
        return None;
    }
    let body = implementation.child_by_field_name("body")?;
    let mut cursor = body.walk();
    let mut bodies = Vec::new();
    for method in body.named_children(&mut cursor) {
        if method.kind() != "function_item"
            || any_node(method, Children::Named, |node| {
                matches!(node.kind(), "function_modifiers" | "abstract_type")
            })
        {
            return None;
        }
        let body = method.child_by_field_name("body")?;
        bodies.push(body.start_byte() + 1..body.end_byte().checked_sub(1)?);
    }
    (!bodies.is_empty()).then_some(bodies)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = "macro_rules! wrap { ($name:ident, $($items:tt)*) => { struct $name; impl $name { $($items)* } }; }\nwrap!(Tile, fn tint(&self) -> u8 { 1 });\npub fn color() -> u8 { Tile.tint() }\n";

    fn hidden(source: &str) -> Vec<Range<usize>> {
        let tree = parser::parse_source(source.as_bytes(), LangId::Rust).expect("Rust");
        ranges(tree.root_node(), source.as_bytes())
    }

    #[test]
    fn private_macro_proof_preserves_source_and_binding_footprint() {
        let changed = SOURCE.replace("{ 1 }", "{ let shade = 2; shade }");
        let index = |source: &str| {
            super::super::DependencyIndex::new(source.as_bytes(), "src/lib.rs").expect("index")
        };
        let old = index(SOURCE);
        let new = index(&changed);
        assert!(!old.private_macro_bodies.is_empty());
        assert_eq!(old.source, SOURCE.as_bytes());
        assert_eq!(new.source, changed.as_bytes());
        assert_eq!(old.fingerprint(), new.fingerprint());
        assert_eq!(old.tree.root_node().end_byte(), SOURCE.len());
        for changed in [
            SOURCE.replace("wrap!(Tile,", "wrap!(Shade,"),
            SOURCE.replace("-> u8 { 1 }", "-> u16 { 1 }"),
            SOURCE.replace("fn tint", "fn other"),
        ] {
            assert_ne!(
                old.fingerprint(),
                index(&changed).fingerprint(),
                "{changed}"
            );
        }
    }

    #[test]
    fn private_macro_proof_rejects_unsupported_or_externally_effective_expansions() {
        for source in [
            SOURCE.replace("struct $name", "pub struct $name"),
            SOURCE.replace("fn tint", "pub fn tint"),
            SOURCE.replace("fn tint", "const fn tint"),
            SOURCE.replace("fn tint", "async fn tint"),
            SOURCE.replace("-> u8", "-> impl Copy"),
            SOURCE.replace(
                "{ 1 }",
                "{ #[macro_export] macro_rules! exported { () => {} } 1 }",
            ),
            SOURCE.replace("{ 1 }", "{ unknown!(); 1 }"),
            SOURCE.replace("struct $name;", "#[derive(Clone)] struct $name;"),
            SOURCE.replace("($name:ident,", "($name:expr,"),
            SOURCE.replace(
                "($name:ident,",
                "($($all:tt)*) => { pub struct Shade; }; ($name:ident,",
            ),
            SOURCE.replace("impl $name", "impl Other"),
            SOURCE.replace("$($items)*", "$($items)+"),
            SOURCE.replace("wrap!(Tile,", "other!(Tile,"),
            SOURCE.replace("wrap!(Tile,", "unknown!(); wrap!(Tile,"),
            SOURCE.replace("wrap!(Tile,", "mod inner {} wrap!(Tile,"),
            SOURCE.replace("wrap!(Tile,", "#[cfg(unix)] wrap!(Tile,"),
            SOURCE.replace("struct $name;", "struct $name; struct Other;"),
            "macro_rules! wrap { ($name:ident, $value:ident, $($items:tt)*) => { struct $name; impl $name { fn tint(&self) -> $value { $value } $($items)* } }; }\nwrap!(Tile, Shade,);\n".to_owned(),
        ] {
            assert!(hidden(&source).is_empty(), "{source}");
        }
        let large = SOURCE.replace(
            "{ 1 }",
            &format!("{{ {} }}", "1 + ".repeat(MAX_PROOF_BYTES / 4)),
        );
        assert!(hidden(&large).is_empty());
        assert!(!hidden(SOURCE).is_empty());
    }

    #[test]
    fn private_macro_proof_node_kinds_exist_in_rust_grammar() {
        let language = LangId::Rust.ts_language();
        for kind in [
            "macro_definition",
            "macro_rule",
            "token_tree",
            "macro_invocation",
            "metavariable",
            "lifetime",
            "string_literal",
            "raw_string_literal",
            "char_literal",
            "attribute_item",
            "inner_attribute_item",
            "visibility_modifier",
            "struct_item",
            "impl_item",
            "type_identifier",
            "function_item",
            "function_modifiers",
            "abstract_type",
            "mod_item",
            "expression_statement",
        ] {
            assert!(
                (0..language.node_kind_count())
                    .any(|id| language.node_kind_for_id(id as u16) == Some(kind)),
                "{kind}"
            );
        }
    }
}
