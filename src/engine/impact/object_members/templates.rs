//! テンプレートの置換を、先に初期化された同一モジュールの定数に限って比較する。

use std::cell::{OnceCell, RefCell};
use std::collections::{HashMap, HashSet};

use tree_sitter::Node;

use super::StaticValue;
use crate::models::symbol::Symbol;

// 旧新で同じ ID を使い、長い定数を置換の出現回数だけ比較し直さない。
pub(super) type LiteralIds = RefCell<HashMap<String, usize>>;
type Literals<'a> = HashMap<&'a str, (usize, usize)>;

pub(super) struct TemplateBindings<'a> {
    root: Node<'a>,
    source: &'a [u8],
    symbols: &'a [Symbol],
    ids: &'a LiteralIds,
    literals: OnceCell<Literals<'a>>,
}

impl<'a> TemplateBindings<'a> {
    pub(super) fn new(
        root: Node<'a>,
        source: &'a [u8],
        symbols: &'a [Symbol],
        ids: &'a LiteralIds,
    ) -> Self {
        Self {
            root,
            source,
            symbols,
            ids,
            literals: OnceCell::new(),
        }
    }

    fn resolve(&self, identifier: Node<'_>) -> Option<usize> {
        if identifier.kind() != "identifier" {
            return None;
        }
        let name = identifier.utf8_text(self.source).ok()?;
        let (end, value) = self.literals.get_or_init(|| self.collect()).get(name)?;
        (*end <= identifier.start_byte()).then_some(*value)
    }

    fn collect(&self) -> Literals<'a> {
        let mut literals = HashMap::new();
        let mut ids = self.ids.borrow_mut();
        for statement in self.root.named_children(&mut self.root.walk()) {
            let lexical = if statement.kind() == "export_statement" {
                let Some(declaration) = statement.child_by_field_name("declaration") else {
                    continue;
                };
                declaration
            } else {
                statement
            };
            if lexical.kind() != "lexical_declaration"
                || !lexical
                    .child_by_field_name("kind")
                    .is_some_and(|kind| kind.kind() == "const")
            {
                continue;
            }
            for decl in lexical.named_children(&mut lexical.walk()) {
                let (Some(name), Some(value)) = (
                    decl.child_by_field_name("name"),
                    decl.child_by_field_name("value"),
                ) else {
                    continue;
                };
                if decl.kind() == "variable_declarator"
                    && name.kind() == "identifier"
                    && decl.child_by_field_name("type").is_none()
                    && let (Ok(name), Ok(text)) =
                        (name.utf8_text(self.source), value.utf8_text(self.source))
                    && plain_primitive(value.kind(), text)
                {
                    let next_id = ids.len();
                    let id = *ids.entry(text.to_owned()).or_insert(next_id);
                    literals.insert(name, (decl.end_byte(), id));
                }
            }
        }
        let mut seen = HashSet::new();
        for symbol in self.symbols {
            let name = symbol.name.as_str();
            if literals.contains_key(name) && !seen.insert(name) {
                literals.remove(name);
            }
        }
        literals
    }
}

fn plain_primitive(kind: &str, text: &str) -> bool {
    match kind {
        "true" | "false" | "null" => true,
        "string" => !text.contains('\\'),
        // strict mode で初期化に失敗する旧式の数値表記を除く。
        "number" => {
            !(text.starts_with('0')
                && text
                    .as_bytes()
                    .get(1)
                    .is_some_and(|c| c.is_ascii_digit() || *c == b'_'))
        }
        _ => false,
    }
}

pub(super) fn template_value(
    node: Node<'_>,
    source: &[u8],
    bindings: &TemplateBindings<'_>,
) -> Option<StaticValue> {
    let mut literals = Vec::new();
    for child in node.named_children(&mut node.walk()) {
        match child.kind() {
            "string_fragment" => {}
            "template_substitution" => {
                let mut cursor = child.walk();
                let mut parts = child
                    .named_children(&mut cursor)
                    .filter(|part| !part.is_extra());
                let value = bindings.resolve(parts.next()?)?;
                if parts.next().is_some() {
                    return None;
                }
                literals.push(value);
            }
            _ => return None,
        }
    }
    Some(StaticValue::Template(
        node.utf8_text(source).ok()?.to_owned(),
        literals,
    ))
}
