//! 名前の綴りや module object 経由で、元名の索引から漏れる利用を保留する。

use std::collections::HashSet;
use std::path::Path;
use std::rc::Rc;

use tree_sitter::Node;

use super::super::{ApiClosureCaches, ModifiedClosureInput};
use crate::language::LangId;

pub(super) fn proves_complete(
    input: &ModifiedClosureInput<'_>,
    caches: &mut ApiClosureCaches,
) -> bool {
    let key = (input.target_new_path.to_owned(), input.name.to_owned());
    if let Some(result) = caches.ts_alias_inventories.get(&key) {
        return *result;
    }
    let result = audit(input, caches).is_some();
    caches.ts_alias_inventories.insert(key, result);
    result
}

pub(in crate::commands::api_changes) fn visit_project_sources(
    dir: &str,
    names: &[&str],
    caches: &mut ApiClosureCaches,
    mut visit: impl FnMut(&str, Node<'_>, &[u8]) -> Option<()>,
) -> Option<()> {
    // 通常のファイル収集が捨てる walk error を、閉包の不在証明では見逃さない。
    let mut entries_left = 100_000usize;
    for entry in ignore::WalkBuilder::new(dir)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .build()
    {
        entries_left = entries_left.checked_sub(1)?;
        let entry = entry.ok()?;
        if entry.file_type().is_some_and(|kind| kind.is_symlink()) {
            return None;
        }
    }
    if super::super::embedded_js_sources_may_hide_removed_keys(dir, names, caches) {
        return None;
    }
    let files = caches
        .ts_checked_files
        .get_or_insert_with(|| {
            crate::engine::refs::collect_files_scan(
                Path::new(dir),
                Some("**/*.{ts,tsx,mts,cts,js,jsx,mjs,cjs}"),
                crate::engine::refs::FileScanOptions::default(),
            )
            .ok()
            .map(|collection| Rc::new(collection.files))
        })
        .clone()?;
    if files.len() > 10_000 {
        return None;
    }
    let root = Path::new(dir);
    let mut bytes_left = 64 * 1024 * 1024usize;
    for path in files.iter() {
        let relative = path.strip_prefix(root).ok()?.to_str()?.replace('\\', "/");
        if !super::plain_path(root, &relative)
            || std::fs::metadata(path).ok()?.len() > 2 * 1024 * 1024
        {
            return None;
        }
        let parsed = super::super::parsed_ref_file_shared(dir, &relative, caches)?;
        if !matches!(
            parsed.lang_id,
            LangId::Javascript | LangId::Typescript | LangId::Tsx
        ) || parsed.source.len() > 2 * 1024 * 1024
            || parsed.tree.root_node().has_error()
        {
            return None;
        }
        bytes_left = bytes_left.checked_sub(parsed.source.len())?;
        visit(&relative, parsed.tree.root_node(), &parsed.source)?;
    }
    Some(())
}

fn audit(input: &ModifiedClosureInput<'_>, caches: &mut ApiClosureCaches) -> Option<()> {
    let mut nodes_left = 100_000usize;
    visit_project_sources(
        input.dir,
        &[input.name],
        caches,
        |relative, root, source| {
            let mut invoked = HashSet::new();
            let mut pending = vec![root];
            while let Some(node) = pending.pop() {
                nodes_left = nodes_left.checked_sub(1)?;
                if crate::engine::js_occurrence::identifier_kind(node.kind()) {
                    let name = node.utf8_text(source).ok()?;
                    if name.contains('\\') {
                        return None;
                    }
                }
                if untracked_loader(node, source, &mut invoked) {
                    return None;
                }
                if opaque_transport(node, source) {
                    let spec = transport_source(node, source)?;
                    let target = super::resolved_relative_source(input.dir, relative, spec)?;
                    if super::super::diff_path_matches_ref(
                        input.target_new_path,
                        &target,
                        input.dir,
                    ) {
                        return None;
                    }
                }
                let mut cursor = node.walk();
                pending.extend(node.named_children(&mut cursor));
            }
            Some(())
        },
    )
}

pub(in crate::commands::api_changes) fn untracked_loader(
    node: Node<'_>,
    source: &[u8],
    invoked: &mut HashSet<usize>,
) -> bool {
    if node.kind() == "call_expression"
        && let Some(loader) = node
            .child_by_field_name("function")
            .and_then(|f| require_callee(f, source))
    {
        invoked.insert(loader.id());
    }
    if crate::engine::js_occurrence::identifier_kind(node.kind())
        && let Ok(raw) = node.utf8_text(source)
        && let Some(name) = crate::engine::js_string::decoded(raw)
    {
        // loader 自体の別名を作ると、その先は静的な import として照合できない。
        return (node.kind() == "identifier" && matches!(name.as_ref(), "eval" | "Function"))
            || (name == "require" && (raw.contains('\\') || !invoked.remove(&node.id())));
    }
    literal_require_member(node, source) && !invoked.remove(&node.id())
}

pub(in crate::commands::api_changes) fn opaque_transport(node: Node<'_>, source: &[u8]) -> bool {
    match node.kind() {
        "import_statement" => {
            let mut cursor = node.walk();
            node.named_children(&mut cursor).any(|part| {
                if part.kind() == "import_clause" {
                    let mut cursor = part.walk();
                    return part
                        .named_children(&mut cursor)
                        .any(|n| !matches!(n.kind(), "named_imports" | "comment"));
                }
                !matches!(part.kind(), "string" | "comment")
            })
        }
        "export_statement" if node.child_by_field_name("source").is_some() => {
            let mut cursor = node.walk();
            !node
                .named_children(&mut cursor)
                .any(|n| n.kind() == "export_clause")
        }
        "call_expression" => node.child_by_field_name("function").is_some_and(|callee| {
            callee.kind() == "import" || require_callee(callee, source).is_some()
        }),
        _ => false,
    }
}

fn require_callee<'a>(callee: Node<'a>, source: &[u8]) -> Option<Node<'a>> {
    if literal_require_member(callee, source) {
        return Some(callee);
    }
    let name = match callee.kind() {
        "identifier" => callee,
        "member_expression" => callee.child_by_field_name("property")?,
        _ => return None,
    };
    (name.utf8_text(source).ok()? == "require").then_some(name)
}

fn literal_require_member(node: Node<'_>, source: &[u8]) -> bool {
    node.kind() == "subscript_expression"
        && node
            .child_by_field_name("index")
            .and_then(|n| super::super::super::static_js_string_text(n, source))
            .is_some_and(|text| {
                crate::engine::js_string::decoded(text).is_none_or(|name| name == "require")
            })
}

pub(in crate::commands::api_changes) fn transport_source<'a>(
    node: Node<'_>,
    source: &'a [u8],
) -> Option<&'a str> {
    let source_node = if node.kind() == "call_expression" {
        let arguments = node.child_by_field_name("arguments")?;
        let mut cursor = arguments.walk();
        let mut values = arguments
            .named_children(&mut cursor)
            .filter(|n| n.kind() != "comment");
        let first = values.next()?;
        if values.next().is_some() {
            return None;
        }
        first
    } else {
        node.child_by_field_name("source")?
    };
    super::super::super::static_js_string_text(source_node, source)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_transport_nodes_exist_and_recognize_real_syntax() {
        for lang in [LangId::Javascript, LangId::Typescript, LangId::Tsx] {
            let language = lang.ts_language();
            for kind in [
                "import_statement",
                "import_clause",
                "named_imports",
                "namespace_import",
                "identifier",
                "export_statement",
                "export_clause",
                "call_expression",
                "member_expression",
                "subscript_expression",
                "import",
                "comment",
            ] {
                tree_sitter::Query::new(&language, &format!("({kind}) @node")).unwrap();
            }
            for source in [
                "import * as api from './pricing';",
                "import api from './pricing';",
                "export * from './pricing';",
                "const api = require('./pricing');",
                "const api = import('./pricing');",
                "const api = module.require('./pricing');",
                "const api = module['require']('./pricing');",
                r"const api = module['requ\u0069re']('./pricing');",
            ] {
                let tree = crate::engine::parser::parse_source(source.as_bytes(), lang).unwrap();
                assert!(!tree.root_node().has_error(), "{source}");
                let mut pending = vec![tree.root_node()];
                let mut found = 0;
                while let Some(node) = pending.pop() {
                    if opaque_transport(node, source.as_bytes()) {
                        found += 1;
                        assert_eq!(transport_source(node, source.as_bytes()), Some("./pricing"));
                    }
                    let mut cursor = node.walk();
                    pending.extend(node.named_children(&mut cursor));
                }
                assert_eq!(found, 1, "{source}");
            }
        }
    }
}
