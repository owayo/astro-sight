//! 同じ root 関数を同じ名前で公開する二つの構文。公開順と関数以外の文も照合する。

use std::collections::{HashMap, HashSet};

use tree_sitter::Node;

use super::{DeclarationChangeInput, LazyOldSide};
use crate::engine::change_evidence::ChangeEvidenceIndex;
use crate::engine::js_occurrence::unique_direct_functions;
use crate::engine::symbols::visit_js_ts_export_clause_specifiers;
use crate::engine::tree_walk::{Children, any_node};
use crate::models::change_evidence::ChangedPart;
use crate::models::impact::AffectedSymbol;

#[derive(PartialEq)]
enum Item<'a> {
    Statement(&'a [u8]),
    Directive(&'a [u8]),
    Export(&'a str, &'a str),
}

#[derive(Default)]
struct Module<'a> {
    statements: Vec<Item<'a>>,
    exports: Vec<Item<'a>>,
    direct: HashMap<&'a str, bool>,
}

fn has_token(node: Node<'_>, kind: &str) -> bool {
    node.children(&mut node.walk())
        .any(|child| child.kind() == kind)
}

impl<'a> Module<'a> {
    fn collect(root: Node<'_>, source: &'a [u8], candidates: &HashSet<&str>) -> Option<Self> {
        if root.has_error()
            || any_node(root, Children::Named, |n| {
                n.kind() == "comment"
                    && n.utf8_text(source).is_ok_and(|c| {
                        c.starts_with("/**") || c.starts_with("///") || c.contains(['@', '#'])
                    })
            })
        {
            return None;
        }
        let mut clauses = HashMap::<_, Vec<_>>::new();
        let mut ambiguous = false;
        visit_js_ts_export_clause_specifiers(root, source, |spec| {
            let local_value = spec.statement.child_by_field_name("source").is_none()
                && !has_token(spec.statement, "type")
                && !has_token(spec.specifier, "type")
                && spec
                    .specifier
                    .child_by_field_name("name")
                    .is_some_and(|n| n.kind() == "identifier")
                && spec
                    .specifier
                    .child_by_field_name("alias")
                    .is_none_or(|n| n.kind() == "identifier");
            if candidates.contains(spec.exported_name)
                && (!local_value || spec.local_name != spec.exported_name)
            {
                ambiguous = true;
            }
            clauses.entry(spec.statement.id()).or_default().push((
                spec.local_name,
                spec.exported_name,
                local_value,
            ));
        });
        if ambiguous {
            return None;
        }
        let mut module = Self::default();
        let mut prologue = true;
        for stmt in root.named_children(&mut root.walk()) {
            if stmt.kind() == "comment" {
                continue;
            }
            let bytes = &source[stmt.byte_range()];
            if stmt.kind() == "hash_bang_line" {
                module.statements.push(Item::Statement(bytes));
                continue;
            }
            let is_directive = prologue
                && stmt.kind() == "expression_statement"
                && stmt.named_child(0).is_some_and(|n| n.kind() == "string");
            prologue = is_directive;
            if stmt.kind() == "export_statement" {
                if let Some(decl) = stmt.child_by_field_name("declaration")
                    && matches!(
                        decl.kind(),
                        "function_declaration" | "generator_function_declaration"
                    )
                    && !has_token(stmt, "default")
                    && let Some(name) = decl.child_by_field_name("name")?.utf8_text(source).ok()
                    && candidates.contains(name)
                {
                    if module.direct.insert(name, true).is_some() {
                        return None;
                    }
                    module
                        .statements
                        .push(Item::Statement(&source[decl.byte_range()]));
                    module.exports.push(Item::Export(name, name));
                    continue;
                }
                if let Some(specs) = clauses.get(&stmt.id())
                    && specs.iter().all(|(_, _, value)| *value)
                {
                    for &(local, public, _) in specs {
                        module.exports.push(Item::Export(local, public));
                        if local == public && candidates.contains(local) {
                            if module.direct.insert(local, false).is_some() {
                                return None;
                            }
                        } else {
                            // 同じ句に並ぶ他の名前の束縛と位置を保つ。
                            module.statements.push(Item::Export(local, public));
                        }
                    }
                    continue;
                }
                module.exports.push(Item::Statement(bytes));
            }
            module.statements.push(if is_directive {
                Item::Directive(bytes)
            } else {
                Item::Statement(bytes)
            });
        }
        Some(module)
    }
}

pub(in crate::engine::impact) fn equivalent_function_exports(
    input: &DeclarationChangeInput<'_>,
    old_side: &LazyOldSide,
    affected: &[AffectedSymbol],
    evidence: &ChangeEvidenceIndex,
) -> HashSet<String> {
    let candidates: HashSet<_> = affected
        .iter()
        .filter(|s| {
            s.kind == "function"
                && s.change_type == "modified"
                && evidence.get(&s.name, &s.kind, input.lang_id).changed_parts
                    == [ChangedPart::Visibility]
        })
        .map(|s| s.name.as_str())
        .collect();
    if candidates.is_empty() {
        return HashSet::new();
    }
    let Some(after) = Module::collect(input.root, input.source, &candidates) else {
        return HashSet::new();
    };
    let Some(before) = old_side
        .get(input)
        .and_then(|old| Module::collect(old.tree.root_node(), &old.source, &candidates))
    else {
        return HashSet::new();
    };
    if before.statements != after.statements || before.exports != after.exports {
        return HashSet::new();
    }
    unique_direct_functions(input.root, input.source, |name| {
        before
            .direct
            .get(name)
            .zip(after.direct.get(name))
            .is_some_and(|(a, b)| a != b)
    })
    .map(str::to_owned)
    .collect()
}
