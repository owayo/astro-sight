//! 値宣言を named export 句へ置き換えた変更の根拠。
//!
//! 直接の再公開は、転送先がリテラル定数だけのモジュールなら旧版の型と値を照合する。

use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;

use tree_sitter::Node;

use crate::engine::tree_walk::{Children, Walk, walk_preorder};
use crate::language::LangId;

pub(super) fn signatures(
    dir: &str,
    path: &str,
    old_source: Option<&[u8]>,
) -> Option<HashMap<String, String>> {
    let source = super::source_pair::load_new_source(dir, path)?;
    let lang = LangId::from_path(camino::Utf8Path::new(path)).ok()?;
    let tree = crate::engine::parser::parse_source(&source, lang).ok()?;
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }
    let mut cursor = root.walk();
    let mut imports = HashMap::new();
    for import in root
        .named_children(&mut cursor)
        .filter(|node| node.kind() == "import_statement")
    {
        super::js_ts_shadow::visit_import_bindings(import, &source, |local| {
            imports.insert(local.to_owned(), import);
            false
        });
    }
    let old = old_source
        .filter(|_| {
            matches!(lang, LangId::Typescript | LangId::Tsx)
                && !super::ts_module_paths::is_declaration_path(path)
                && !source.windows(9).any(|s| s == b"@internal")
        })
        .and_then(|s| literal_exports(s, lang, false));
    let mut targets = HashMap::new();
    let mut proven = HashSet::new();
    let mut result = HashMap::new();
    let local_reads = old.as_ref().and_then(|_| local_names(root, &source));
    let mut cursor = root.walk();
    for statement in root.named_children(&mut cursor) {
        if statement.kind() != "export_statement" {
            continue;
        }
        let source_node = statement.child_by_field_name("source");
        let type_only = has_type_token(statement);
        let module = source_node
            .filter(|_| {
                !type_only
                    && statement
                        .named_children(&mut statement.walk())
                        .all(|n| matches!(n.kind(), "export_clause" | "string" | "comment"))
            })
            .and_then(|n| super::static_js_string_text(n, &source));
        let mut cursor = statement.walk();
        for clause in statement.named_children(&mut cursor) {
            if clause.kind() != "export_clause" {
                continue;
            }
            let mut cursor = clause.walk();
            for specifier in clause.named_children(&mut cursor) {
                if specifier.kind() != "export_specifier" {
                    continue;
                }
                let local = specifier.child_by_field_name("name")?;
                let local_name = local.utf8_text(&source).ok()?;
                if source_node.is_none() && !imports.contains_key(local_name) {
                    // 同じファイルのローカル宣言は既存のシグネチャ比較に委ねる。
                    continue;
                }
                let external = specifier.child_by_field_name("alias").unwrap_or(local);
                let name = external.utf8_text(&source).ok()?.to_owned();
                let spec = super::signature::normalize_signature_dropping_comments(
                    specifier, &source, true,
                )?;
                let mut signature = format!(
                    "export {}{{ {spec} }}",
                    if type_only { "type " } else { "" },
                );
                if let Some(node) = source_node {
                    signature.push_str(" from ");
                    signature.push_str(node.utf8_text(&source).ok()?);
                }
                if let Some(module) = module
                    && let Some(literal) = old.as_ref().and_then(|old| old.get(&name))
                    && !has_type_token(specifier)
                    && local.kind() == "identifier"
                    && external.kind() == "identifier"
                    && !result.contains_key(&name)
                    && local_reads.as_ref().is_some_and(|reads| {
                        !reads.contains(name.as_str())
                            || imports.get(&name).is_some_and(|import| {
                                imports_value(*import, &source, module, local_name, &name)
                            })
                    })
                    && targets
                        .entry(module)
                        .or_insert_with(|| target_literals(dir, path, module))
                        .as_ref()
                        .and_then(|target| target.get(local_name))
                        == Some(literal)
                {
                    proven.insert(name.clone());
                } else {
                    proven.remove(&name);
                }
                result.insert(name, signature);
            }
        }
    }
    result.retain(|name, _| !proven.contains(name));
    Some(result)
}

fn has_type_token(node: Node<'_>) -> bool {
    node.children(&mut node.walk()).any(|n| n.kind() == "type")
}

/// import と from 付きの再公開はローカルの値を読まない。それ以外の同名出現には import を求める。
fn local_names<'s>(root: Node<'_>, source: &'s [u8]) -> Option<HashSet<&'s str>> {
    let mut names = HashSet::new();
    let walked = walk_preorder(root, Children::Named, |n, _| {
        if n.kind() == "import_statement"
            || n.kind() == "export_statement" && n.child_by_field_name("source").is_some()
        {
            return ControlFlow::Continue(Walk::Skip);
        }
        if crate::engine::js_occurrence::identifier_kind(n.kind())
            && let Ok(text) = n.utf8_text(source)
        {
            if text.contains('\\') || matches!(text, "eval" | "Function") {
                return ControlFlow::Break(());
            }
            names.insert(text);
        }
        ControlFlow::Continue(Walk::Descend)
    });
    walked.is_continue().then_some(names)
}

fn imports_value(
    statement: Node<'_>,
    source: &[u8],
    module: &str,
    remote: &str,
    local: &str,
) -> bool {
    !has_type_token(statement)
        && statement
            .child_by_field_name("source")
            .and_then(|n| super::static_js_string_text(n, source))
            == Some(module)
        && crate::engine::tree_walk::any_node(statement, Children::Named, |n| {
            n.kind() == "import_specifier"
                && !has_type_token(n)
                && n.child_by_field_name("name").is_some_and(|name| {
                    name.utf8_text(source).ok() == Some(remote)
                        && n.child_by_field_name("alias")
                            .unwrap_or(name)
                            .utf8_text(source)
                            .ok()
                            == Some(local)
                })
        })
}

type LiteralExports = HashMap<String, (Option<String>, String)>;

fn target_literals(dir: &str, path: &str, module: &str) -> Option<LiteralExports> {
    let file = module.strip_prefix("./")?;
    if file.is_empty()
        || file
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || file.contains(['\\', ':', '?', '#', '%'])
        || super::removed_attribution::relative_directory_has_manifest(dir, path, module)
    {
        return None;
    }
    let target = super::ref_index::resolved_relative_source(dir, path, module)?;
    if target == path || super::ts_module_paths::is_declaration_path(&target) {
        return None;
    }
    let lang = LangId::from_path(camino::Utf8Path::new(&target)).ok()?;
    if !matches!(lang, LangId::Typescript | LangId::Tsx) {
        return None;
    }
    let source = super::source_pair::load_new_source(dir, &target)?;
    literal_exports(&source, lang, true)
}

/// 転送先では全ての文を確かめ、循環 import やモジュール評価時の副作用を伴う移動を除く。
fn literal_exports(source: &[u8], lang: LangId, standalone: bool) -> Option<LiteralExports> {
    let tree = crate::engine::parser::parse_source(source, lang).ok()?;
    let root = tree.root_node();
    if root.has_error() || source.windows(9).any(|s| s == b"@internal") {
        return None;
    }
    let mut exports = HashMap::new();
    for stmt in root
        .named_children(&mut root.walk())
        .filter(|n| n.kind() != "comment")
    {
        if let Some((name, annotation, value)) = literal_const(stmt, source) {
            if exports
                .insert(
                    name.to_owned(),
                    (annotation.map(str::to_owned), value.to_owned()),
                )
                .is_some()
            {
                return None;
            }
        } else if standalone {
            return None;
        }
    }
    Some(exports)
}

fn literal_const<'a>(
    statement: Node<'_>,
    source: &'a [u8],
) -> Option<(&'a str, Option<&'a str>, &'a str)> {
    let decl = statement.child_by_field_name("declaration")?;
    if statement.kind() != "export_statement"
        || decl.kind() != "lexical_declaration"
        || decl.child_by_field_name("kind")?.kind() != "const"
    {
        return None;
    }
    let mut cursor = decl.walk();
    let mut items = decl
        .named_children(&mut cursor)
        .filter(|n| n.kind() != "comment");
    let item = items.next()?;
    let name = item.child_by_field_name("name")?;
    let value = item.child_by_field_name("value")?;
    if items.next().is_some()
        || name.kind() != "identifier"
        || !matches!(
            value.kind(),
            "string" | "number" | "true" | "false" | "null"
        )
    {
        return None;
    }
    let annotation = match item.child_by_field_name("type") {
        None => None,
        Some(ty) => Some(
            ty.named_child(0)
                .filter(|n| n.kind() == "predefined_type")?
                .utf8_text(source)
                .ok()?,
        ),
    };
    Some((
        name.utf8_text(source).ok()?,
        annotation,
        value.utf8_text(source).ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_are_per_external_name_and_keep_local_declarations_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api.ts");
        std::fs::write(&path, "import { Remote as LOCAL } from './next';\nimport DEFAULT from './default';\nimport * as NS from './namespace';\nconst own = 1; export { own };\nexport { LOCAL as VALUE, DEFAULT, NS };\nexport type { Shape } from './types';\nexport { A, B as Other } from './more';\n").unwrap();
        let root = dir.path().to_str().unwrap();
        let result = signatures(root, "api.ts", None).unwrap();
        assert_eq!(result.len(), 6);
        assert!(!result.contains_key("own") && !result.contains_key("Remote"));
        assert_eq!(result["VALUE"], "export { LOCAL as VALUE }");
        assert_eq!(result["DEFAULT"], "export { DEFAULT }");
        assert_eq!(result["NS"], "export { NS }");
        assert_eq!(result["Shape"], "export type { Shape } from './types'");
        assert_eq!(result["A"], "export { A } from './more'");
        assert_eq!(result["Other"], "export { B as Other } from './more'");
        // 兄弟の specifier が変わっても A の根拠は同じ。
        std::fs::write(&path, "export { A, C } from './more';").unwrap();
        assert_eq!(signatures(root, "api.ts", None).unwrap()["A"], result["A"]);
        std::fs::write(&path, "export { A } from './more'; const = ;").unwrap();
        assert!(signatures(root, "api.ts", None).is_none());
        assert!(signatures(root, "missing.ts", None).is_none());
    }
}
