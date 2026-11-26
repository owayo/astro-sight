//! 同じ差分でローカル const を廃止し、新しい export const から直接 import した参照を照合する。

use std::collections::HashMap;
use std::path::Path;

use camino::Utf8Path;
use tree_sitter::Node;

use super::js_const::unique_top_level_const;
use super::static_const::{ScalarKind, js_scalar_literal_kind};
use super::ts_const_arg::{collect_bindings_named, declarator_is_const};
use crate::engine::change_evidence::{MAX_EVIDENCE_SOURCE_BYTES, has_js_unicode_escape};
use crate::engine::js_scope::visit_import_bindings;
use crate::engine::{diff, parser};
use crate::language::LangId;
use crate::models::impact::DiffFile;

/// ディレクトリの package.json と設定を含め、別名が一意なファイルを指すか確かめる。
pub(crate) fn resolved_const_import(dir: &str, from: &str, specifier: &str) -> Option<String> {
    super::removed_attribution::normalize_relative_specifier(from, specifier)?;
    if super::removed_attribution::relative_directory_has_manifest(dir, from, specifier) {
        return None;
    }
    if directory_specifier(specifier) {
        return super::ref_index::resolved_relative_source(
            dir,
            from,
            &format!("{specifier}/index"),
        );
    }
    super::ref_index::resolved_relative_source(dir, from, specifier)
}

pub(crate) fn directory_specifier(specifier: &str) -> bool {
    matches!(specifier.rsplit('/').next(), Some("" | "." | ".."))
}

#[derive(Default)]
pub(crate) struct AddedExports {
    by_name: HashMap<String, HashMap<String, Literal>>,
}

struct Literal {
    kind: ScalarKind,
    text: String,
}

pub(crate) enum Rebinding {
    SameLiteral,
    DifferentLiteral,
}

pub(crate) type ReboundCallers = HashMap<String, HashMap<String, Rebinding>>;

impl Rebinding {
    pub(crate) fn covers(&self, node: Node<'_>) -> bool {
        if !matches!(node.kind(), "identifier" | "shorthand_property_identifier") {
            return false;
        }
        if matches!(self, Self::SameLiteral) {
            return true;
        }
        // 値が違えば、呼び出し引数や型注釈で旧リテラル型を要求している可能性がある。
        let Some(substitution) = node
            .parent()
            .filter(|part| part.kind() == "template_substitution")
        else {
            return false;
        };
        let Some(template) = substitution
            .parent()
            .filter(|part| part.kind() == "template_string")
        else {
            return false;
        };
        template.parent().is_some_and(|var| {
            var.kind() == "variable_declarator"
                && var.child_by_field_name("value") == Some(template)
                && var.child_by_field_name("type").is_none()
                && declarator_is_const(var)
        })
    }
}

impl AddedExports {
    pub(crate) fn record(&mut self, path: &str, name: &str, root: Node<'_>, source: &[u8]) {
        if let Some(literal) = const_literal(root, source, name, true) {
            self.by_name
                .entry(name.to_owned())
                .or_default()
                .insert(path.to_owned(), literal);
        }
    }

    pub(crate) fn rebound_callers(
        &self,
        dir: &Path,
        files: &[DiffFile],
        sections: &diff::FileSections<'_>,
        read: impl Fn(&str) -> Option<parser::SourceBuf>,
    ) -> HashMap<String, ReboundCallers> {
        let mut callers = HashMap::<String, ReboundCallers>::new();
        let Some(dir) = dir.to_str().filter(|_| !self.by_name.is_empty()) else {
            return callers;
        };
        for file in files {
            let path = &file.new_path;
            if path != &file.old_path || super::ts_module_paths::is_declaration_path(path) {
                continue;
            }
            let Ok(lang @ (LangId::Typescript | LangId::Tsx)) =
                LangId::from_path(Utf8Path::new(path))
            else {
                continue;
            };
            let Some(new) = read(path) else { continue };
            let Some(new_tree) = checked_tree(&new, lang) else {
                continue;
            };
            let imports = self.named_imports(new_tree.root_node(), &new, dir, path);
            if imports.is_empty() {
                continue;
            }
            let Some(old) = diff::reconstruct_old_source(&sections.get(path), path, &new) else {
                continue;
            };
            let Some(old_tree) = checked_tree(&old.source, lang) else {
                continue;
            };
            let root = old_tree.root_node();
            // スクリプト直下はグローバルな束縛になり得る。
            if !root
                .named_children(&mut root.walk())
                .any(|node| matches!(node.kind(), "export_statement" | "import_statement"))
            {
                continue;
            }
            for (name, target, new_value) in imports {
                if let Some(old_value) = const_literal(root, &old.source, name, false)
                    && old_value.kind == new_value.kind
                {
                    let change = if old_value.text == new_value.text {
                        Rebinding::SameLiteral
                    } else {
                        Rebinding::DifferentLiteral
                    };
                    callers
                        .entry(target)
                        .or_default()
                        .entry(path.clone())
                        .or_default()
                        .insert(name.to_owned(), change);
                }
            }
        }
        callers
    }

    fn named_imports<'s, 'a>(
        &'a self,
        root: Node<'_>,
        source: &'s [u8],
        dir: &str,
        path: &str,
    ) -> Vec<(&'s str, String, &'a Literal)> {
        let mut imports = Vec::new();
        for statement in root
            .named_children(&mut root.walk())
            .filter(|node| node.kind() == "import_statement")
        {
            if statement
                .children(&mut statement.walk())
                .any(|part| matches!(part.kind(), "type" | "import_attribute"))
            {
                continue;
            }
            let Some(module) = statement
                .child_by_field_name("source")
                .and_then(|node| super::static_js_string_text(node, source))
            else {
                continue;
            };
            let Some(names) = statement
                .named_children(&mut statement.walk())
                .find(|part| part.kind() == "import_clause")
                .and_then(|clause| {
                    clause
                        .named_children(&mut clause.walk())
                        .find(|group| group.kind() == "named_imports")
                })
            else {
                continue;
            };
            let mut binding_counts = HashMap::new();
            visit_import_bindings(statement, source, |local| {
                *binding_counts.entry(local.to_owned()).or_insert(0) += 1;
                false
            });
            let target = std::cell::OnceCell::new();
            for specifier in names.named_children(&mut names.walk()) {
                let Some(name) = specifier
                    .child_by_field_name("name")
                    .and_then(|node| node.utf8_text(source).ok())
                else {
                    continue;
                };
                let Some(origins) = self.by_name.get(name) else {
                    continue;
                };
                if specifier.kind() != "import_specifier"
                    || specifier.utf8_text(source) != Ok(name)
                    || binding_counts.get(name) != Some(&1)
                {
                    continue;
                }
                let Some(target) = target.get_or_init(|| resolved_const_import(dir, path, module))
                else {
                    continue;
                };
                let Some(literal) = origins.get(target) else {
                    continue;
                };
                // import 文とその specifier 以外に同名の束縛・書換えがあれば証明しない。
                let bindings = collect_bindings_named(root, source, name);
                if bindings
                    .iter()
                    .any(|binding| binding.id() == statement.id())
                    && bindings.iter().all(|binding| {
                        binding.id() == statement.id() || binding.id() == specifier.id()
                    })
                {
                    imports.push((name, target.clone(), literal));
                }
            }
        }
        imports
    }
}

fn checked_tree(source: &[u8], lang: LangId) -> Option<tree_sitter::Tree> {
    if source.len() > MAX_EVIDENCE_SOURCE_BYTES
        || has_js_unicode_escape(source)
        || std::str::from_utf8(source).is_err()
    {
        return None;
    }
    parser::parse_source(source, lang)
        .ok()
        .filter(|tree| !tree.root_node().has_error())
}

/// 再輸出や CommonJS の文があると、旧版が外部へ提供した名前を構文木だけで列挙できない。
pub(crate) fn has_direct_exports_only(root: Node<'_>) -> bool {
    root.named_children(&mut root.walk()).all(|node| {
        matches!(
            node.kind(),
            "import_statement"
                | "lexical_declaration"
                | "function_declaration"
                | "class_declaration"
                | "type_alias_declaration"
                | "interface_declaration"
                | "comment"
        ) || (node.kind() == "export_statement"
            && node.child_by_field_name("declaration").is_some())
    })
}

fn const_literal(root: Node<'_>, source: &[u8], name: &str, exported: bool) -> Option<Literal> {
    let (declaration, statement) = unique_top_level_const(root, source, name)?;
    if (statement.kind() == "export_statement") != exported
        || declaration.child_by_field_name("type").is_some()
        || (!exported && collect_bindings_named(root, source, name).len() != 1)
    {
        return None;
    }
    let value = declaration.child_by_field_name("value")?;
    match value.kind() {
        "string"
            if !value
                .named_children(&mut value.walk())
                .all(|child| super::js_const::is_template_text(child, source)) =>
        {
            return None;
        }
        "number"
            if value
                .utf8_text(source)
                .ok()?
                .strip_prefix('0')
                .and_then(|rest| rest.as_bytes().first())
                .is_some_and(|b| b.is_ascii_digit() || *b == b'_') =>
        {
            return None;
        }
        _ => {}
    }
    Some(Literal {
        kind: js_scalar_literal_kind(value, source)
            .filter(|kind| *kind != ScalarKind::Undefined)?,
        text: value.utf8_text(source).ok()?.to_owned(),
    })
}
