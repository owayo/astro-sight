//! 直接の named import に閉じた別名の参照を、閉包の追加義務として集める (Issue #163)。

use std::collections::HashSet;
use std::path::{Component, Path};

use tree_sitter::Node;

use super::{ApiClosureCaches, ClosureArgEvidence, ModifiedClosureInput, RefImportRole};
use crate::engine::js_scope::{
    Namespace, ScopeBinding, resolve_binding_scope, visit_import_bindings,
};
use crate::language::LangId;
use crate::models::reference::{RefKind, SymbolReference};

mod inventory;

pub(super) struct AliasedCall {
    pub(super) local_name: String,
    pub(super) reference: SymbolReference,
}

pub(super) fn collect(
    input: &ModifiedClosureInput<'_>,
    refs: &[SymbolReference],
    caches: &mut ApiClosureCaches,
) -> Option<Vec<AliasedCall>> {
    let bare = super::bare_name(input.name);
    let mut calls = Vec::new();
    let mut seen = HashSet::new();
    let mut remaining = 100_000usize;
    for reference in refs {
        if reference.kind == Some(RefKind::Definition)
            || super::ref_import_role(input.dir, reference, bare, caches)
                != Some(RefImportRole::Aliased)
        {
            continue;
        }
        if input.kind != "function"
            || !input.js_direct_function_change
            || input.name.contains('.')
            || !plain_name(bare)
            || !matches!(input.arg_evidence, ClosureArgEvidence::None)
        {
            return None;
        }
        let parsed = super::parsed_ref_file_shared(input.dir, &reference.path, caches)?;
        let source = &parsed.source;
        let root = parsed.tree.root_node();
        if !matches!(parsed.lang_id, LangId::Typescript | LangId::Tsx)
            || root.has_error()
            || source.len() > 2 * 1024 * 1024
        {
            return None;
        }
        let name = super::ref_name_node(&parsed, bare, reference.line, reference.column)?;
        let specifier = name.parent().filter(|n| n.kind() == "import_specifier")?;
        if specifier.child_by_field_name("name").map(|n| n.id()) != Some(name.id())
            || has_type_token(specifier)
        {
            return None;
        }
        let alias = specifier
            .child_by_field_name("alias")
            .filter(|n| n.kind() == "identifier")?;
        let local_name = alias.utf8_text(source).ok()?;
        if !plain_name(local_name) || local_name == bare {
            return None;
        }
        let mut statement = specifier;
        while statement.kind() != "import_statement" {
            statement = statement.parent()?;
        }
        if statement.parent().map(|n| n.id()) != Some(root.id()) || has_type_token(statement) {
            return None;
        }
        if !seen.insert((reference.path.clone(), alias.start_byte())) {
            continue;
        }
        if seen.len() > 64 || !import_is_target(input, &reference.path, statement, source) {
            return None;
        }
        // 同じ module で重複する import binding は字句 resolver の先勝ちに任せない。
        let mut bindings = 0;
        let mut cursor = root.walk();
        for child in root
            .named_children(&mut cursor)
            .filter(|n| n.kind() == "import_statement")
        {
            let escaped = visit_import_bindings(child, source, |name| {
                bindings += usize::from(name == local_name);
                name.contains('\\')
            });
            if escaped {
                return None;
            }
        }
        if bindings != 1 {
            return None;
        }
        let mut pending = vec![root];
        while let Some(node) = pending.pop() {
            remaining = remaining.checked_sub(1)?;
            if matches!(
                node.kind(),
                "with_statement" | "internal_module" | "ambient_declaration"
            ) {
                return None;
            }
            if node.kind() == "import_statement" {
                continue;
            }
            if matches!(
                node.kind(),
                "identifier"
                    | "type_identifier"
                    | "shorthand_property_identifier"
                    | "shorthand_property_identifier_pattern"
            ) {
                let text = node.utf8_text(source).ok()?;
                if text.contains('\\') || text == "eval" {
                    return None;
                }
                if text == local_name {
                    let (scope, binding) =
                        resolve_binding_scope(node, source, local_name, Namespace::Value)?;
                    match binding {
                        ScopeBinding::Import(owner) if owner.id() == statement.id() => {
                            if !direct_call(node) {
                                return None;
                            }
                            calls.push(AliasedCall {
                                local_name: local_name.to_owned(),
                                reference: SymbolReference {
                                    path: reference.path.clone(),
                                    line: node.start_position().row,
                                    column: node.start_position().column,
                                    context: None,
                                    kind: Some(RefKind::Reference),
                                    confidence: None,
                                },
                            });
                        }
                        ScopeBinding::Import(_) => return None,
                        _ if scope.kind() != "program" => {}
                        _ => return None,
                    }
                }
            }
            let mut cursor = node.walk();
            // 結果の順序はソース順に固定する。
            let start = pending.len();
            pending.extend(node.named_children(&mut cursor));
            pending[start..].reverse();
        }
    }
    if !seen.is_empty() && !inventory::proves_complete(input, caches) {
        return None;
    }
    Some(calls)
}

fn plain_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().enumerate().all(|(i, b)| {
            b == b'_' || b == b'$' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit())
        })
}

fn has_type_token(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| child.kind() == "type")
}

fn direct_call(mut node: Node<'_>) -> bool {
    if node.kind() != "identifier" {
        return false;
    }
    while let Some(parent) = node
        .parent()
        .filter(|n| n.kind() == "parenthesized_expression")
    {
        node = parent;
    }
    node.parent().is_some_and(|call| {
        call.kind() == "call_expression"
            && call.child_by_field_name("function").map(|n| n.id()) == Some(node.id())
    })
}

fn import_is_target(
    input: &ModifiedClosureInput<'_>,
    from: &str,
    statement: Node<'_>,
    source: &[u8],
) -> bool {
    let mut cursor = statement.walk();
    if statement
        .named_children(&mut cursor)
        .any(|n| !matches!(n.kind(), "import_clause" | "string" | "comment"))
    {
        return false;
    }
    let Some(spec) = statement
        .child_by_field_name("source")
        .and_then(|n| super::super::static_js_string_text(n, source))
    else {
        return false;
    };
    resolved_relative_source(input, from, spec).is_some_and(|target| {
        super::diff_path_matches_ref(input.target_new_path, &target, input.dir)
            && !super::diff_path_matches_ref(input.target_new_path, from, input.dir)
    })
}

fn resolved_relative_source(
    input: &ModifiedClosureInput<'_>,
    from: &str,
    spec: &str,
) -> Option<String> {
    if spec.contains('\\') {
        return None;
    }
    let candidates = super::super::removed_attribution::relative_import_candidates(from, spec)?;
    let root = Path::new(input.dir);
    let mut existing = Vec::new();
    for candidate in candidates {
        match std::fs::symlink_metadata(root.join(&candidate)) {
            Ok(metadata) if metadata.file_type().is_symlink() => return None,
            Ok(metadata) if metadata.is_file() => existing.push(candidate),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
    }
    let [target] = existing.as_slice() else {
        return None;
    };
    (plain_path(root, from) && plain_path(root, target) && plain_configs(root, from))
        .then(|| target.clone())
}

fn plain_path(root: &Path, relative: &str) -> bool {
    let mut path = root.to_owned();
    let mut paths = vec![path.clone()];
    for component in Path::new(relative).components() {
        let Component::Normal(part) = component else {
            return false;
        };
        path.push(part);
        paths.push(path.clone());
    }
    paths.iter().all(|path| {
        std::fs::symlink_metadata(path).is_ok_and(|metadata| {
            if metadata.file_type().is_symlink() {
                return false;
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if metadata.file_attributes() & 0x400 != 0 {
                    return false;
                }
            }
            true
        })
    })
}

fn plain_configs(root: &Path, from: &str) -> bool {
    let mut directory = Path::new(from).parent();
    let mut remaining = 10_000usize;
    for _ in 0..128 {
        let relative = directory.unwrap_or_else(|| Path::new(""));
        let Ok(entries) = std::fs::read_dir(root.join(relative)) else {
            return false;
        };
        for entry in entries {
            let Some(next) = remaining.checked_sub(1) else {
                return false;
            };
            remaining = next;
            let Ok(entry) = entry else {
                return false;
            };
            let filename = entry.file_name();
            let Some(name) = filename.to_str() else {
                return false;
            };
            if !(name.starts_with("tsconfig") || name.starts_with("jsconfig"))
                || !name.ends_with(".json")
            {
                continue;
            }
            let relative_config = relative.join(name).to_string_lossy().replace('\\', "/");
            if !plain_path(root, &relative_config) {
                return false;
            }
            let Some(text) =
                crate::engine::bounded_read::read_utf8_file_limited(&entry.path(), 1024 * 1024)
            else {
                return false;
            };
            if ["rootDirs", "moduleSuffixes", "extends", "\\"]
                .iter()
                .any(|key| text.contains(key))
            {
                return false;
            }
        }
        if relative.as_os_str().is_empty() {
            return true;
        }
        directory = relative.parent();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_node_kinds_and_import_fields_exist_in_both_ts_grammars() {
        for lang in [LangId::Typescript, LangId::Tsx] {
            let language = lang.ts_language();
            for kind in [
                "import_statement",
                "import_specifier",
                "identifier",
                "type_identifier",
                "shorthand_property_identifier",
                "shorthand_property_identifier_pattern",
                "with_statement",
                "internal_module",
                "ambient_declaration",
                "parenthesized_expression",
                "call_expression",
                "import_clause",
                "string",
                "comment",
            ] {
                tree_sitter::Query::new(&language, &format!("({kind}) @node")).unwrap();
            }
            tree_sitter::Query::new(
                &language,
                "(import_specifier name: (identifier) @name alias: (identifier) @alias)",
            )
            .unwrap();
        }
    }

    #[test]
    fn alias_resolution_rejects_non_workspace_paths_and_opaque_configs() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("cart.ts"), "").unwrap();
        assert!(plain_path(temp.path(), "cart.ts"));
        assert!(!plain_path(temp.path(), "../cart.ts"));
        assert!(plain_configs(temp.path(), "cart.ts"));
        std::fs::write(temp.path().join("tsconfig.json"), [0xff]).unwrap();
        assert!(!plain_configs(temp.path(), "cart.ts"));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(temp.path().join("cart.ts"), temp.path().join("linked.ts"))
                .unwrap();
            assert!(!plain_path(temp.path(), "linked.ts"));
        }
    }
}
