//! TS 関数契約のトークン比較と、直接定義した const callable の解決。

use tree_sitter::Node;

use super::super::signature_tokens::{SigTokens, signature_tokens_in_range};
use super::{TS_ONLY_LANGS, with_resolved_ts_fn_pair};
use crate::commands::api_changes::js_const::unique_top_level_const;
use crate::commands::api_changes::source_pair::{CompatibleModSite, SignatureSourceCache};

fn unwrap_parentheses(mut value: Node<'_>) -> Option<Node<'_>> {
    while value.kind() == "parenthesized_expression" {
        let mut cursor = value.walk();
        let expressions: Vec<_> = value
            .named_children(&mut cursor)
            .filter(|child| child.kind() != "comment")
            .collect();
        let [expression] = expressions.as_slice() else {
            return None;
        };
        value = *expression;
    }
    Some(value)
}

fn ts_site_languages(
    site: &CompatibleModSite<'_>,
) -> Option<(crate::language::LangId, crate::language::LangId)> {
    let old = crate::language::LangId::from_path(camino::Utf8Path::new(site.old_path)).ok()?;
    let new = crate::language::LangId::from_path(camino::Utf8Path::new(site.new_path)).ok()?;
    (TS_ONLY_LANGS.contains(&old) && TS_ONLY_LANGS.contains(&new)).then_some((old, new))
}

struct ConstCallable<'tree> {
    function: Node<'tree>,
    prefix: SigTokens,
    suffix: SigTokens,
}

/// 括弧以外の assertion/wrapper を剥がさず、公開型の変更を推測しない。
fn const_callable<'tree>(
    root: Node<'tree>,
    source: &[u8],
    name: &str,
) -> Option<ConstCallable<'tree>> {
    let (declarator, statement) = unique_top_level_const(root, source, name)?;
    let function = unwrap_parentheses(declarator.child_by_field_name("value")?)?;
    if !matches!(function.kind(), "arrow_function" | "function_expression") {
        return None;
    }
    Some(ConstCallable {
        function,
        prefix: signature_tokens_in_range(
            statement,
            source,
            statement.start_byte(),
            function.start_byte(),
        )?,
        suffix: signature_tokens_in_range(
            statement,
            source,
            function.end_byte(),
            statement.end_byte(),
        )?,
    })
}

fn same_const_declaration(old: &ConstCallable<'_>, new: &ConstCallable<'_>) -> bool {
    old.function.kind() == new.function.kind()
        && old.prefix == new.prefix
        && old.suffix == new.suffix
}

/// 任意引数/任意 object property の追加だけに使う。その他の JSX 判定器へは広げない。
pub(super) fn with_resolved_ts_callable_pair<T>(
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
    check: impl FnOnce(Node<'_>, &[u8], Node<'_>, &[u8]) -> Option<T>,
) -> Option<T> {
    if !matches!(site.kind, "variable" | "constant") {
        return with_resolved_ts_fn_pair(site, sources, check);
    }
    site.lang_in(TS_ONLY_LANGS)?;
    let (old_lang, new_lang) = ts_site_languages(site)?;
    if site.name.is_empty() || site.name.contains('.') {
        return None;
    }
    let src = sources.get(site)?;
    let old_tree = crate::engine::parser::parse_source(&src.old, old_lang).ok()?;
    let new_tree = crate::engine::parser::parse_source(&src.new, new_lang).ok()?;
    let old = const_callable(old_tree.root_node(), &src.old, site.name)?;
    let new = const_callable(new_tree.root_node(), &src.new, site.name)?;
    if !same_const_declaration(&old, &new) {
        return None;
    }
    check(old.function, &src.old, new.function, &src.new)
}

fn is_direct_callable(value: Node<'_>) -> bool {
    matches!(
        value.kind(),
        "arrow_function" | "function_expression" | "generator_function"
    )
}

/// signature 抽出は初期化子の外形を変えない。解決前に non-direct を除き、
/// companion type や複数 declarator の object/wrapper 値を巻き込まない。
/// パース不能な signature は元ソースの検証へ回す。
fn signature_direct_initializer(
    signature: &str,
    name: &str,
    lang: crate::language::LangId,
) -> Option<bool> {
    let source = signature.as_bytes();
    let tree = crate::engine::parser::parse_source(source, lang).ok()?;
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }
    // 出力 signature は declarator 単位だが、兄弟を含む入力でも対象名だけを選ぶ。
    // 初期化子内のローカル束縛へ降りず、型などの companion 宣言もここでは解決しない。
    let mut found = None;
    let mut cursor = root.walk();
    for statement in root.named_children(&mut cursor) {
        let declaration = if statement.kind() == "export_statement" {
            statement.child_by_field_name("declaration")?
        } else {
            statement
        };
        if !matches!(
            declaration.kind(),
            "lexical_declaration" | "variable_declaration"
        ) {
            continue;
        }
        let mut declaration_cursor = declaration.walk();
        for declarator in declaration.named_children(&mut declaration_cursor) {
            if declarator.kind() != "variable_declarator" {
                continue;
            }
            let binding = declarator.child_by_field_name("name")?;
            if binding.kind() != "identifier" || binding.utf8_text(source).ok()? != name {
                continue;
            }
            if found.replace(declarator).is_some() {
                return None;
            }
        }
    }
    let value = unwrap_parentheses(found?.child_by_field_name("value")?)?;
    Some(is_direct_callable(value))
}

/// シグネチャ出力は空白正規化済みなので、const_value の型注釈は元ソースで再確認する。
/// 任意 wrapper の分類は変えず、直接定義した TS callable が含まれる候補だけを検証する。
pub(in crate::commands::api_changes) fn ts_const_value_header_guard(
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
) -> bool {
    let Some(site_lang) = site.lang_in(TS_ONLY_LANGS) else {
        return true;
    };
    let languages = ts_site_languages(site);
    // JS → TS などの rename でも、明確な non-direct 値の既存分類は維持する。
    // 直接 callable / 不明な形だけは両側の TS 文法による元ソース証明を要求する。
    let (old_parse, new_parse) = languages.unwrap_or((site_lang, site_lang));
    if signature_direct_initializer(site.old_sig, site.name, old_parse) == Some(false)
        && signature_direct_initializer(site.new_sig, site.name, new_parse) == Some(false)
    {
        return true;
    }
    let Some((old_lang, new_lang)) = languages else {
        return false;
    };
    let mut check = || -> Option<bool> {
        let src = sources.get(site)?;
        let old_tree = crate::engine::parser::parse_source(&src.old, old_lang).ok()?;
        let new_tree = crate::engine::parser::parse_source(&src.new, new_lang).ok()?;
        let (old, old_statement) =
            unique_top_level_const(old_tree.root_node(), &src.old, site.name)?;
        let (new, new_statement) =
            unique_top_level_const(new_tree.root_node(), &src.new, site.name)?;
        let old_value = old.child_by_field_name("value")?;
        let new_value = new.child_by_field_name("value")?;
        let direct = |value| unwrap_parentheses(value).is_some_and(is_direct_callable);
        if !direct(old_value) && !direct(new_value) {
            return Some(true);
        }
        if old.child_by_field_name("type").is_none() || new.child_by_field_name("type").is_none() {
            return Some(false);
        }
        // 一意な単一 declarator の const だけなので、ヘッダに兄弟の初期化子は混入しない。
        let old_header = signature_tokens_in_range(
            old_statement,
            &src.old,
            old_statement.start_byte(),
            old_value.start_byte(),
        )?;
        let new_header = signature_tokens_in_range(
            new_statement,
            &src.new,
            new_statement.start_byte(),
            new_value.start_byte(),
        )?;
        Some(old_header == new_header)
    };
    check().unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::super::super::signature_tokens::node_signature_tokens;
    use super::*;
    use crate::{engine::parser, language::LangId};

    #[test]
    fn const_callable_rejects_ambiguous_bindings_and_indirect_values() {
        for lang in [LangId::Typescript, LangId::Tsx] {
            for source in [
                "export const f = (x: number) => x;",
                "export const f = function named(x: number) { return x; };",
                "export const f: Fn = ((/* same */ x => x));",
                "const f = () => 1; export { f };",
            ] {
                let tree = parser::parse_source(source.as_bytes(), lang).unwrap();
                assert!(
                    const_callable(tree.root_node(), source.as_bytes(), "f").is_some(),
                    "{source}"
                );
            }
            for source in [
                "export let f = x => x;",
                "export var f = x => x;",
                "export const f = x => x, other = 1;",
                "function outer() { const f = x => x; }",
                "export const f = wrap(x => x);",
                "export const f = (x => x) as Fn;",
                "export const f = (x => x) satisfies Fn;",
                "export const f = (x => x)!;",
                "export const f = (() => x => x)();",
                "export const f = function* () {};",
            ] {
                let tree = parser::parse_source(source.as_bytes(), lang).unwrap();
                assert!(
                    const_callable(tree.root_node(), source.as_bytes(), "f").is_none(),
                    "{source}"
                );
            }
            for other in [
                "const f = 1;",
                "type f = number;",
                "interface f {}",
                "namespace f.Inner {}",
                "declare const f: Fn;",
                "import { other as f } from './other';",
                "f = other;",
                "const broken = ;",
            ] {
                let source = format!("export const f = x => x;\n{other}");
                let tree = parser::parse_source(source.as_bytes(), lang).unwrap();
                assert!(
                    const_callable(tree.root_node(), source.as_bytes(), "f").is_none(),
                    "{source}"
                );
            }
        }
    }

    #[test]
    fn const_callable_keeps_outer_annotation_parentheses_and_function_kind() {
        let old = "export const f: Fn<'a b'> = (x => x);";
        let old_tree = parser::parse_source(old.as_bytes(), LangId::Typescript).unwrap();
        let old_callable = const_callable(old_tree.root_node(), old.as_bytes(), "f").unwrap();
        for (new, same) in [
            (
                "export const f: Fn<'a b'> = ((x, extra = 1) => x + extra);",
                true,
            ),
            ("export const f: Fn<'a  b'> = ((x, extra = 1) => x);", false),
            ("export const f: Other = ((x, extra = 1) => x);", false),
            ("export const f: Fn<'a b'> = (x, extra = 1) => x;", false),
            (
                "export const f: Fn<'a b'> = (function(x, extra = 1) { return x; });",
                false,
            ),
            (
                "export const f: Fn<'a b'> = ((x, extra = 1) => x) /* changed */;",
                false,
            ),
        ] {
            let tree = parser::parse_source(new.as_bytes(), LangId::Typescript).unwrap();
            let new_callable = const_callable(tree.root_node(), new.as_bytes(), "f").unwrap();
            assert_eq!(
                same_const_declaration(&old_callable, &new_callable),
                same,
                "{new}"
            );
        }
    }

    #[test]
    fn signature_tokens_preserve_literal_contents_and_comments() {
        for (old, new) in [
            ("'a b'", "'a  b'"),
            ("`a b`", "`a  b`"),
            ("/a b/", "/a  b/"),
            ("type T = `a b${string}`;", "type T = `a  b${string}`;"),
            ("/* a b */ x", "/* a  b */ x"),
            ("<><b>a</b> <i /></>", "<><b>a</b><i /></>"),
        ] {
            let a = parser::parse_source(old.as_bytes(), LangId::Tsx).unwrap();
            let b = parser::parse_source(new.as_bytes(), LangId::Tsx).unwrap();
            let a = node_signature_tokens(a.root_node(), old.as_bytes()).unwrap();
            let b = node_signature_tokens(b.root_node(), new.as_bytes()).unwrap();
            assert_ne!(a, b, "{old}");
        }
        let old = "const f = (x:number) => x;";
        let new = "const f = ( x : number ) => x ;";
        let a = parser::parse_source(old.as_bytes(), LangId::Typescript).unwrap();
        let b = parser::parse_source(new.as_bytes(), LangId::Typescript).unwrap();
        assert_eq!(
            node_signature_tokens(a.root_node(), old.as_bytes()),
            node_signature_tokens(b.root_node(), new.as_bytes())
        );
        assert!(
            signature_tokens_in_range(a.root_node(), old.as_bytes(), 0, old.len() + 1).is_none()
        );
        let literal = "'a b'";
        let tree = parser::parse_source(literal.as_bytes(), LangId::Typescript).unwrap();
        assert!(signature_tokens_in_range(tree.root_node(), literal.as_bytes(), 1, 4).is_none());
        let broken = "const f = ;";
        let tree = parser::parse_source(broken.as_bytes(), LangId::Typescript).unwrap();
        assert!(node_signature_tokens(tree.root_node(), broken.as_bytes()).is_none());
    }

    #[test]
    fn non_direct_initializer_guard_does_not_load_original_sources() {
        for signature in [
            "export const f: Fn = wrap(x => x);",
            "export const f: Map = { a: x => x };",
            "export const f: string = '=> function';",
            "export const f: Fn = (x => x) as Fn;",
        ] {
            assert_eq!(
                signature_direct_initializer(signature, "f", LangId::Typescript),
                Some(false)
            );
            let site = CompatibleModSite {
                dir: "missing-directory",
                base: "HEAD",
                old_path: "api.ts",
                new_path: "api.ts",
                name: "f",
                kind: "variable",
                old_sig: signature,
                new_sig: signature,
                lang_id: Some(LangId::Typescript),
            };
            // 元ソースを読めば失敗する site で true = non-direct は I/O を行わない。
            assert!(ts_const_value_header_guard(
                &site,
                &mut SignatureSourceCache::default()
            ));
        }
        for signature in [
            "export const f: Fn = x => x;",
            "export const f: Fn = (function(x) {});",
        ] {
            assert_eq!(
                signature_direct_initializer(signature, "f", LangId::Typescript),
                Some(true)
            );
        }
        assert_eq!(
            signature_direct_initializer("export const f = (", "f", LangId::Typescript),
            None
        );
        for lang in [LangId::Typescript, LangId::Tsx] {
            for (signature, expected) in [
                ("export const other = 1, f = x => x;", Some(true)),
                ("export const f = 1, other = x => x;", Some(false)),
                ("const f = 1; const f = x => x;", None),
                ("const other = 1;", None),
                ("const f = wrap(() => { const f = 1; });", Some(false)),
            ] {
                assert_eq!(signature_direct_initializer(signature, "f", lang), expected);
            }
            let site = CompatibleModSite {
                dir: "missing-directory",
                base: "HEAD",
                old_path: "api.js",
                new_path: "api.ts",
                name: "f",
                kind: "variable",
                old_sig: "export const f = 1;",
                new_sig: "export const f = 2;",
                lang_id: Some(lang),
            };
            assert!(ts_const_value_header_guard(
                &site,
                &mut SignatureSourceCache::default()
            ));
        }
    }

    fn parts(value: &str) -> Option<super::super::TsFunctionSignatureParts> {
        let source = format!("export const f = {value};");
        let tree = parser::parse_source(source.as_bytes(), LangId::Typescript).unwrap();
        if tree.root_node().has_error() {
            return None;
        }
        let callable = const_callable(tree.root_node(), source.as_bytes(), "f").unwrap();
        super::super::ts_function_signature_parts(callable.function, source.as_bytes())
    }

    #[test]
    fn bare_arrow_is_one_required_parameter_not_an_empty_list() {
        let old = parts("value => value").unwrap();
        assert_eq!(old.params.len(), 1);
        assert!(!old.params[0].omittable);
        let new = parts("(value, extra = 1) => value + extra").unwrap();
        assert_eq!(old.head, new.head);
        assert_eq!(old.tail, new.tail);
        assert!(super::super::ts_params_prefix_same_with_optional_tail(
            &old.params,
            &new.params
        ));
        for value in [
            "(renamed?: number) => renamed",
            "(value: number, extra = 1) => value",
        ] {
            let new = parts(value).unwrap();
            assert!(
                !super::super::ts_params_prefix_same_with_optional_tail(&old.params, &new.params),
                "{value}"
            );
        }
    }

    #[test]
    fn parser_accepted_rest_tails_do_not_prove_compatibility() {
        // tree-sitter は rest の後続引数を ERROR 無しで受け付けるため、独立した guard が要る。
        for value in [
            "(...rest: number[], extra?: number) => 0",
            "(...rest: number[], extra = 1) => 0",
        ] {
            let source = format!("export const f = {value};");
            let tree = parser::parse_source(source.as_bytes(), LangId::Typescript).unwrap();
            assert!(!tree.root_node().has_error(), "{source}");
        }
        for value in [
            "(...rest: number[], extra?: number) => 0",
            "(...rest: number[], extra = 1) => 0",
            "(...rest?: number[]) => 0",
            "(...rest: number[] = []) => 0",
        ] {
            assert!(parts(value).is_none(), "{value}");
        }
        let valid =
            parts("(options: { size: number }, ...rest: number[]) => options.size").unwrap();
        assert_eq!(valid.params.len(), 2);
        assert!(!valid.params[1].omittable);
    }
}
