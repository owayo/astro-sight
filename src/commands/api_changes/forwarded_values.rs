//! 値宣言を named export 句へ置き換えた変更の根拠。
//!
//! 転送先の型・値・モジュール評価は解決しない。同名が公開され続けるだけでは
//! 契約の維持を証明できないため、呼び出し側は未検証の変更として報告する。

use std::collections::HashMap;

use crate::language::LangId;

pub(super) fn signatures(dir: &str, path: &str) -> Option<HashMap<String, String>> {
    let source = super::source_pair::load_new_source(dir, path)?;
    let lang = LangId::from_path(camino::Utf8Path::new(path)).ok()?;
    let tree = crate::engine::parser::parse_source(&source, lang).ok()?;
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }
    let mut cursor = root.walk();
    let mut imports = std::collections::HashSet::new();
    for import in root
        .named_children(&mut cursor)
        .filter(|node| node.kind() == "import_statement")
    {
        super::js_ts_shadow::visit_import_bindings(import, &source, |local| {
            imports.insert(local.to_owned());
            false
        });
    }
    let mut result = HashMap::new();
    let mut cursor = root.walk();
    for statement in root.named_children(&mut cursor) {
        if statement.kind() != "export_statement" {
            continue;
        }
        let source_node = statement.child_by_field_name("source");
        let mut cursor = statement.walk();
        let type_only = statement.children(&mut cursor).any(|n| n.kind() == "type");
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
                if source_node.is_none() && !imports.contains(local_name) {
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
                result.insert(name, signature);
            }
        }
    }
    Some(result)
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
        let result = signatures(root, "api.ts").unwrap();
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
        assert_eq!(signatures(root, "api.ts").unwrap()["A"], result["A"]);
        std::fs::write(&path, "export { A } from './more'; const = ;").unwrap();
        assert!(signatures(root, "api.ts").is_none());
        assert!(signatures(root, "missing.ts").is_none());
    }
}
