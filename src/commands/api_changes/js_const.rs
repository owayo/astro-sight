//! 一意なトップレベル const と、任意 wrapper の callback 本体変更の証拠。

use tree_sitter::Node;

use super::source_pair::{CompatibleModSite, SignatureSourceCache};
use super::ts_const_arg::{collect_bindings_named, declarator_is_const};

/// 本文内のローカル束縛と、module の同名宣言を区別する。
fn is_module_binding(mut node: Node<'_>) -> bool {
    while let Some(parent) = node.parent() {
        match parent.kind() {
            "program" => return true,
            "lexical_declaration"
            | "variable_declaration"
            | "export_statement"
            | "ambient_declaration"
            | "import_statement"
            | "expression_statement" => node = parent,
            _ => return false,
        }
    }
    false
}

/// (declarator, 宣言全体) を返す。型・import・namespace 等の同名宣言も曖昧性に含める。
/// 複数 declarator、分割代入、ambient 宣言、構文エラーは証明対象外。
pub(super) fn unique_top_level_const<'a>(
    root: Node<'a>,
    source: &[u8],
    name: &str,
) -> Option<(Node<'a>, Node<'a>)> {
    if root.has_error() {
        return None;
    }
    let bindings: Vec<_> = collect_bindings_named(root, source, name)
        .into_iter()
        .filter(|node| is_module_binding(*node))
        .collect();
    let [declarator] = bindings.as_slice() else {
        return None;
    };
    let declarator = *declarator;
    if declarator.kind() != "variable_declarator"
        || !declarator_is_const(declarator)
        || declarator.child_by_field_name("name")?.kind() != "identifier"
    {
        return None;
    }
    let declaration = declarator.parent()?;
    let mut cursor = declaration.walk();
    if declaration
        .named_children(&mut cursor)
        .filter(|c| c.kind() == "variable_declarator")
        .count()
        != 1
    {
        return None;
    }
    let parent = declaration.parent()?;
    let statement = match parent.kind() {
        "program" => declaration,
        "export_statement"
            if parent.parent()?.kind() == "program"
                && parent.child_by_field_name("declaration")?.id() == declaration.id() =>
        {
            parent
        }
        _ => return None,
    };
    Some((declarator, statement))
}

fn unwrap_transparent_expression(mut node: Node<'_>) -> Node<'_> {
    loop {
        let inner = match node.kind() {
            "parenthesized_expression"
            | "as_expression"
            | "satisfies_expression"
            | "non_null_expression" => node.named_child(0),
            "type_assertion" => {
                let mut cursor = node.walk();
                node.named_children(&mut cursor).last()
            }
            _ => return node,
        };
        match inner {
            Some(inner) => node = inner,
            None => return node,
        }
    }
}

/// 引数に直接置かれた関数だけを辿る。object/array、IIFE の callee、new/await は辿らない。
fn callback_body_spans(value: Node<'_>, out: &mut Vec<(usize, usize)>) -> bool {
    let value = unwrap_transparent_expression(value);
    if value.kind() != "call_expression" {
        return false;
    }
    let Some(args) = value
        .child_by_field_name("arguments")
        .filter(|n| n.kind() == "arguments")
    else {
        return false; // tagged template は callback 引数ではない。
    };
    if let Some(callee) = value.child_by_field_name("function") {
        // カリー化された呼び出しだけを辿り、関数そのものを実行する IIFE は省かない。
        callback_body_spans(callee, out);
    }
    let mut cursor = args.walk();
    for arg in args.named_children(&mut cursor) {
        let arg = unwrap_transparent_expression(arg);
        match arg.kind() {
            "arrow_function" | "function_expression" | "generator_function" => {
                if let Some(body) = arg.child_by_field_name("body") {
                    out.push((body.start_byte(), body.end_byte()));
                }
            }
            "call_expression" => {
                callback_body_spans(arg, out);
            }
            _ => {}
        }
    }
    true
}

struct CallbackScaffold {
    tokens: Vec<String>,
    bodies: Vec<blake3::Hash>,
}

fn callback_scaffold(root: Node<'_>, source: &[u8], name: &str) -> Option<CallbackScaffold> {
    let (declarator, statement) = unique_top_level_const(root, source, name)?;
    let mut spans = Vec::new();
    if !callback_body_spans(declarator.child_by_field_name("value")?, &mut spans)
        || spans.is_empty()
    {
        return None;
    }
    // 宣言全体を AST トークンで比較し、const/export/型注釈/callee/非関数引数も保持する。
    // 全文の split_whitespace は文字列引数の内部まで同一視してしまうため使わない。
    let bodies = spans
        .iter()
        .map(|&(start, end)| source.get(start..end).map(blake3::hash))
        .collect::<Option<Vec<_>>>()?;
    let spans: std::collections::HashSet<_> = spans.into_iter().collect();
    let mut tokens = Vec::new();
    let mut stack = vec![statement];
    while let Some(node) = stack.pop() {
        if spans.contains(&(node.start_byte(), node.end_byte())) {
            tokens.push("{}".to_string());
        } else if node.kind() == "comment" {
            let text = node.utf8_text(source).ok()?;
            if text.starts_with("/**") {
                tokens.push(text.to_string());
            }
        } else if node.child_count() == 0
            || matches!(node.kind(), "string" | "template_string" | "regex")
            || node.kind().starts_with("jsx_")
        {
            tokens.push(node.utf8_text(source).ok()?.to_string());
        } else {
            let mut cursor = node.walk();
            stack.extend(
                node.children(&mut cursor)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev(),
            );
        }
    }
    Some(CallbackScaffold { tokens, bodies })
}

pub(super) fn detect_callback_body_only_change(
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
) -> bool {
    let Some(lang) = site.lang_id.filter(|lang| {
        matches!(
            lang,
            crate::language::LangId::Javascript
                | crate::language::LangId::Typescript
                | crate::language::LangId::Tsx
        )
    }) else {
        return false;
    };
    if !matches!(site.kind, "constant" | "variable") || site.name.contains('.') {
        return false;
    }
    let Some(src) = sources.get(site) else {
        return false;
    };
    let Some((old_tree, new_tree)) = src.parse_pair(lang) else {
        return false;
    };
    match (
        callback_scaffold(old_tree.root_node(), &src.old, site.name),
        callback_scaffold(new_tree.root_node(), &src.new, site.name),
    ) {
        // 本文が無変更なら、骨格の整形だけを callback 本文変更とは報告しない。
        (Some(old), Some(new)) => old.tokens == new.tokens && old.bodies != new.bodies,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{engine::parser, language::LangId};

    fn scaffold(source: &str, lang: LangId) -> Option<Vec<String>> {
        let tree = parser::parse_source(source.as_bytes(), lang).unwrap();
        callback_scaffold(tree.root_node(), source.as_bytes(), "handle")
            .map(|scaffold| scaffold.tokens)
    }

    #[test]
    fn callback_bodies_are_elided_without_claiming_runtime_compatibility() {
        for lang in [LangId::Javascript, LangId::Typescript, LangId::Tsx] {
            for value in [
                "wrap(async req => { return BODY; })",
                "wrap(async function (req) { return BODY; })",
                "wrap(function handle(req) { return BODY; })",
                "wrap(function* named(req) { yield BODY; })",
                "wrap(wrap(req => BODY))",
                "factory(req => BODY)(() => BODY)",
                "wrap((() => BODY))",
                "create(set => ({ a: BODY, b: 2 }))",
            ] {
                let before = format!("export const handle = {};", value.replace("BODY", "1"));
                let after = format!("export const handle = {};", value.replace("BODY", "2"));
                let old = scaffold(&before, lang).expect(&before);
                assert_eq!(Some(old), scaffold(&after, lang), "{lang:?}: {value}");
            }
        }
        for value in [
            "(wrap((req: Request): Response => BODY)) as Handler",
            "wrap((req: Request) => BODY) satisfies Handler",
            "wrap(() => BODY)!",
            "<Handler>wrap(() => BODY)",
            "wrap(((() => BODY) as Handler))",
        ] {
            let old = format!("export const handle = {};", value.replace("BODY", "1"));
            let new = format!("export const handle = {};", value.replace("BODY", "2"));
            assert_eq!(
                scaffold(&old, LangId::Typescript).expect(&old),
                scaffold(&new, LangId::Typescript).expect(&new)
            );
        }
    }

    #[test]
    fn callback_headers_and_call_scaffolding_are_preserved() {
        let before = "export const handle = wrap<Request>((req: Request): Response => { return 1; }, { retry: 1 });";
        let old = scaffold(before, LangId::Typescript).unwrap();
        for after in [
            "export const handle = wrap<Request>((req: Request, ctx: unknown): Response => { return 2; }, { retry: 1 });",
            "export const handle = wrap<Request>((req: Request): Other => { return 2; }, { retry: 1 });",
            "export const handle = wrap<Other>((req: Request): Response => { return 2; }, { retry: 1 });",
            "export const handle = other<Request>((req: Request): Response => { return 2; }, { retry: 1 });",
            "export const handle = wrap<Request>((req: Request): Response => { return 2; }, { retry: 2 });",
            "export const handle: Handler = wrap<Request>((req: Request): Response => { return 2; }, { retry: 1 });",
            "const handle = wrap<Request>((req: Request): Response => { return 2; }, { retry: 1 });",
            "export const handle = wrap<Request>(async (req: Request): Response => { return 2; }, { retry: 1 });",
        ] {
            assert_ne!(
                Some(&old),
                scaffold(after, LangId::Typescript).as_ref(),
                "{after}"
            );
        }
        for (old, new) in [
            (
                "wrap(function old() { return 1; })",
                "wrap(function renamed() { return 2; })",
            ),
            (
                "wrap(() => 1, { fn: () => 1 })",
                "wrap(() => 2, { fn: () => 2 })",
            ),
            ("wrap((() => 1) as A)", "wrap((() => 2) as B)"),
        ] {
            let old = format!("export const handle = {old};");
            let new = format!("export const handle = {new};");
            assert_ne!(
                scaffold(&old, LangId::Typescript),
                scaffold(&new, LangId::Typescript)
            );
        }
        for (old_arg, new_arg) in [
            ("'a b'", "'a  b'"),
            ("`a b`", "`a  b`"),
            ("/a b/", "/a  b/"),
        ] {
            let old = format!("export const handle = wrap(() => 1, {old_arg});");
            let new = format!("export const handle = wrap(() => 2, {new_arg});");
            assert_ne!(
                scaffold(&old, LangId::Typescript),
                scaffold(&new, LangId::Typescript)
            );
        }
        let old = "export const handle = wrap(() => 1, <><b>a</b> <i /></>);";
        let new = "export const handle = wrap(() => 2, <><b>a</b><i /></>);";
        assert_ne!(scaffold(old, LangId::Tsx), scaffold(new, LangId::Tsx));
        let source = "export const handle = wrap(() => <><b>a</b> <i /></>);";
        let changed = "export const handle = wrap(() => <><b>a</b><i /></>);";
        assert_eq!(
            scaffold(source, LangId::Tsx),
            scaffold(changed, LangId::Tsx)
        );
    }

    #[test]
    fn callback_proof_rejects_ambiguous_bindings_and_other_expression_positions() {
        for value in [
            "wrap({ fn: () => 1 })",
            "wrap([() => 1])",
            "(() => 1)()",
            "new Wrapper(() => 1)",
            "await wrap(() => 1)",
            "wrap(() => 1).result",
            "wrap`text`",
        ] {
            let source = format!("export const handle = {value};");
            assert!(scaffold(&source, LangId::Typescript).is_none(), "{source}");
        }
        let binding = "export const handle = wrap(() => 1);";
        for other in [
            "const handle = 2;",
            "function handle() {}",
            "class handle {}",
            "type handle = number;",
            "interface handle {}",
            "namespace handle.Inner {}",
            "declare const handle: Handler;",
            "import { other as handle } from './other';",
            "import handle = require('./other');",
            "const { x: handle } = object;",
            "handle = other;",
            "const = ;",
        ] {
            let source = format!("{binding}\n{other}");
            assert!(scaffold(&source, LangId::Typescript).is_none(), "{source}");
        }
        for source in [
            "export let handle = wrap(() => 1);",
            "export var handle = wrap(() => 1);",
            "export const handle = wrap(() => 1), other = 2;",
            "function outer() { const handle = wrap(() => 1); }",
            "declare const handle: Handler;",
        ] {
            assert!(scaffold(source, LangId::Typescript).is_none(), "{source}");
        }
        // 関数ローカルの同名束縛は module の宣言と競合しない。
        assert!(
            scaffold(
                "export const handle = wrap(handle => handle);",
                LangId::Typescript
            )
            .is_some()
        );
    }
}
