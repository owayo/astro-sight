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

/// call の object 引数に置かれた関数値だけの本体を集める。
/// pair の key、array、spread、値にある call や class には進まない。
fn object_callback_body_spans(object: Node<'_>, out: &mut Vec<(usize, usize)>) {
    let mut stack = vec![object];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "object" => {
                let mut cursor = node.walk();
                stack.extend(
                    node.named_children(&mut cursor)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev(),
                );
            }
            "pair" => {
                let Some(value) = node.child_by_field_name("value") else {
                    continue;
                };
                let value = unwrap_transparent_expression(value);
                match value.kind() {
                    "arrow_function" | "function_expression" | "generator_function" => {
                        if let Some(body) = value.child_by_field_name("body") {
                            out.push((body.start_byte(), body.end_byte()));
                        }
                    }
                    "object" => stack.push(value),
                    _ => {}
                }
            }
            "method_definition" => {
                let Some(name) = node.child_by_field_name("name") else {
                    continue;
                };
                let mut cursor = node.walk();
                let accessor = node
                    .children(&mut cursor)
                    .take_while(|child| child.id() != name.id())
                    .any(|child| matches!(child.kind(), "get" | "set"));
                if !accessor
                    && let Some(body) = node
                        .child_by_field_name("body")
                        .filter(|body| body.kind() == "statement_block")
                {
                    out.push((body.start_byte(), body.end_byte()));
                }
            }
            _ => {}
        }
    }
}

#[derive(Default)]
struct CallbackEvidence<'a> {
    bodies: Vec<(usize, usize)>,
    templates: Vec<Node<'a>>,
}

/// 引数に直接置かれた関数と tagged template だけを辿る。
/// object 引数は関数値だけを辿り、array、IIFE の callee、new/await は辿らない。
fn callback_body_spans<'a>(value: Node<'a>, out: &mut CallbackEvidence<'a>) -> bool {
    let value = unwrap_transparent_expression(value);
    if value.kind() != "call_expression" {
        return false;
    }
    let Some(args) = value
        .child_by_field_name("arguments")
        .filter(|n| n.kind() == "arguments")
    else {
        return false; // 宣言の値そのものが tagged template なら対象外。
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
                    out.bodies.push((body.start_byte(), body.end_byte()));
                }
            }
            "call_expression" => match arg.child_by_field_name("arguments") {
                Some(template) if template.kind() == "template_string" => {
                    out.templates.push(template);
                }
                _ => {
                    callback_body_spans(arg, out);
                }
            },
            "object" => object_callback_body_spans(arg, &mut out.bodies),
            _ => {}
        }
    }
    true
}

/// `${...}` の外側にある静的テキストを、空の部分も含めてバイト範囲で返す。
/// escape_sequence の境界には依存せず、埋め込み式の AST ノードだけを境界とする。
struct TemplateGaps<'a> {
    ranges: Vec<(usize, usize)>,
    substitutions: Vec<Node<'a>>,
}

fn template_gaps<'a>(template: Node<'a>, source: &[u8]) -> Option<TemplateGaps<'a>> {
    let start = template.start_byte();
    let end = template.end_byte();
    if end < start + 2 || source.get(start) != Some(&b'`') || source.get(end - 1) != Some(&b'`') {
        return None;
    }
    let mut gaps = Vec::new();
    let mut substitutions = Vec::new();
    let mut gap_start = start + 1;
    let mut cursor = template.walk();
    for child in template.named_children(&mut cursor) {
        match child.kind() {
            "string_fragment" | "escape_sequence" => {}
            "template_substitution" => {
                if child.start_byte() < gap_start || child.end_byte() > end - 1 {
                    return None;
                }
                gaps.push((gap_start, child.start_byte()));
                gap_start = child.end_byte();
                substitutions.push(child);
            }
            _ => return None,
        }
    }
    if gap_start > end - 1 {
        return None;
    }
    gaps.push((gap_start, end - 1));
    Some(TemplateGaps {
        ranges: gaps,
        substitutions,
    })
}

struct CallbackScaffold {
    tokens: Vec<String>,
    bodies: Vec<blake3::Hash>,
    gaps: Vec<blake3::Hash>,
}

fn callback_scaffold(root: Node<'_>, source: &[u8], name: &str) -> Option<CallbackScaffold> {
    let (declarator, statement) = unique_top_level_const(root, source, name)?;
    let mut evidence = CallbackEvidence::default();
    if !callback_body_spans(declarator.child_by_field_name("value")?, &mut evidence)
        || (evidence.bodies.is_empty() && evidence.templates.is_empty())
    {
        return None;
    }
    // 宣言全体を AST トークンで比較し、const/export/型注釈/callee/非関数引数も保持する。
    // 全文の split_whitespace は文字列引数の内部まで同一視してしまうため使わない。
    let bodies = evidence
        .bodies
        .iter()
        .map(|&(start, end)| source.get(start..end).map(blake3::hash))
        .collect::<Option<Vec<_>>>()?;
    let spans: std::collections::HashSet<_> = evidence.bodies.into_iter().collect();
    let mut gaps = Vec::new();
    let mut templates = std::collections::HashMap::new();
    for template in evidence.templates {
        let template_gaps = template_gaps(template, source)?;
        for (start, end) in template_gaps.ranges {
            gaps.push(blake3::hash(source.get(start..end)?));
        }
        templates.insert(template.id(), template_gaps.substitutions);
    }
    enum Item<'a> {
        Node(Node<'a>),
        Token(&'static str),
    }
    let mut tokens = Vec::new();
    let mut stack = vec![Item::Node(statement)];
    while let Some(item) = stack.pop() {
        let node = match item {
            Item::Node(node) => node,
            Item::Token(token) => {
                tokens.push(token.to_string());
                continue;
            }
        };
        if spans.contains(&(node.start_byte(), node.end_byte())) {
            tokens.push("{}".to_string());
        } else if let Some(substitutions) = templates.get(&node.id()) {
            stack.push(Item::Token("\0template_close"));
            for substitution in substitutions.iter().rev() {
                stack.push(Item::Token("\0template_gap"));
                stack.push(Item::Node(*substitution));
            }
            stack.push(Item::Token("\0template_gap"));
            stack.push(Item::Token("\0template_open"));
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
                    .map(Item::Node)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev(),
            );
        }
    }
    Some(CallbackScaffold {
        tokens,
        bodies,
        gaps,
    })
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
        (Some(old), Some(new)) => {
            old.tokens == new.tokens && (old.bodies != new.bodies || old.gaps != new.gaps)
        }
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
    fn tagged_template_static_text_is_elided_but_substitutions_are_preserved() {
        let before = "export const handle = view('totals').as(q`select ${name} from items`);";
        let old = scaffold(before, LangId::Typescript).expect("tagged template scaffold");
        for after in [
            "export const handle = view('totals').as(q`SELECT ${name} from items`);",
            "export const handle = view('totals').as(q`select ${name} from active_items`);",
            "export const handle = view('totals').as(q`select\\n${name} from items`);",
        ] {
            assert_eq!(Some(&old), scaffold(after, LangId::Typescript).as_ref());
        }
        for after in [
            "export const handle = view('totals').as(q`select ${other} from items`);",
            "export const handle = view('totals').as(q`select ${name}${other} from items`);",
            "export const handle = view('totals').as(sql`select ${name} from items`);",
            "export const handle = other('totals').as(q`select ${name} from items`);",
            "export const handle = view('totals').as(`select ${name} from items`);",
        ] {
            assert_ne!(Some(&old), scaffold(after, LangId::Typescript).as_ref());
        }
        for lang in [LangId::Javascript, LangId::Typescript, LangId::Tsx] {
            for (before, after) in [
                (
                    "export const handle = wrap(q``);",
                    "export const handle = wrap(q`text`);",
                ),
                (
                    "export const handle = wrap(q`${a}${b}`);",
                    "export const handle = wrap(q`before${a}middle${b}after`);",
                ),
                (
                    "export const handle = wrap(q`\\n${name}`);",
                    "export const handle = wrap(q`\\t${name}`);",
                ),
            ] {
                let old = scaffold(before, lang).expect(before);
                assert_eq!(
                    Some(old),
                    scaffold(after, lang),
                    "{lang:?}: {before} -> {after}"
                );
            }
            assert_ne!(
                scaffold("export const handle = wrap(q`${a}${b}`);", lang),
                scaffold("export const handle = wrap(q`${b}${a}`);", lang)
            );
        }
        assert!(scaffold("export const handle = q`select items`;", LangId::Typescript).is_none());
    }

    #[test]
    fn object_argument_callback_bodies_are_elided_with_headers_preserved() {
        for value in [
            "wrap({ fn: async (item: Item) => { return BODY; } })",
            "wrap({ fn: function (item: Item) { return BODY; } })",
            "wrap({ fn(item: Item) { return BODY; } })",
            "wrap({ nested: { fn(item: Item) { return BODY; } } })",
        ] {
            let before = format!("export const handle = {};", value.replace("BODY", "1"));
            let after = format!("export const handle = {};", value.replace("BODY", "2"));
            let old = scaffold(&before, LangId::Typescript).expect(&before);
            assert_eq!(Some(old), scaffold(&after, LangId::Typescript), "{value}");
        }
        let before = "export const handle = wrap<Settings>({ fn: async (item: Item) => { return 1; }, stable: 2 });";
        let old = scaffold(before, LangId::Typescript).expect(before);
        for after in [
            "export const handle = wrap<Settings>({ other: async (item: Item) => { return 2; }, stable: 2 });",
            "export const handle = wrap<Settings>({ fn: async (value: Item) => { return 2; }, stable: 2 });",
            "export const handle = wrap<Settings>({ fn: async (item: Other) => { return 2; }, stable: 2 });",
            "export const handle = wrap<Settings>({ fn: [async (item: Item) => { return 2; }], stable: 2 });",
            "export const handle = wrap<Other>({ fn: async (item: Item) => { return 2; }, stable: 2 });",
            "export const handle = wrap<Settings>({ fn: async (item: Item) => { return 2; }, stable: 3 });",
        ] {
            assert_ne!(
                Some(&old),
                scaffold(after, LangId::Typescript).as_ref(),
                "{after}"
            );
        }
        let before = "export const handle = wrap(() => 1, { fn: () => 1 });";
        let after = "export const handle = wrap(() => 2, { fn: () => 2 });";
        assert_eq!(
            scaffold(before, LangId::Typescript),
            scaffold(after, LangId::Typescript)
        );

        for (before, after) in [
            (
                "export const handle = wrap({ fn: () => 1, get x() { return 1; } });",
                "export const handle = wrap({ fn: () => 1, get x() { return 2; } });",
            ),
            (
                "export const handle = wrap({ fn: () => 1, set x(value) { this.x = value; } });",
                "export const handle = wrap({ fn: () => 1, set x(value) { this.x = value + 1; } });",
            ),
            (
                "export const handle = wrap({ fn: () => 1, ...base });",
                "export const handle = wrap({ fn: () => 1, ...other });",
            ),
            (
                "export const handle = wrap({ fn: () => 1, list: [() => 1] });",
                "export const handle = wrap({ fn: () => 1, list: [() => 2] });",
            ),
            (
                "export const handle = wrap({ fn: () => 1, other: inner(() => 1) });",
                "export const handle = wrap({ fn: () => 1, other: inner(() => 2) });",
            ),
            (
                "export const handle = wrap({ fn: () => 1, item: class { method() { return 1; } } });",
                "export const handle = wrap({ fn: () => 1, item: class { method() { return 2; } } });",
            ),
        ] {
            let old = scaffold(before, LangId::Typescript).expect(before);
            assert_ne!(Some(old), scaffold(after, LangId::Typescript), "{after}");
        }

        let before = "export const handle = wrap({ fn(item: Item): Result { return 1; } });";
        let old = scaffold(before, LangId::Typescript).expect(before);
        for after in [
            "export const handle = wrap({ other(item: Item): Result { return 2; } });",
            "export const handle = wrap({ fn(value: Item): Result { return 2; } });",
            "export const handle = wrap({ fn(item: Other): Result { return 2; } });",
            "export const handle = wrap({ fn(item: Item): Other { return 2; } });",
            "export const handle = wrap({ async fn(item: Item): Result { return 2; } });",
            "export const handle = wrap({ *fn(item: Item): Result { return 2; } });",
        ] {
            assert_ne!(
                Some(&old),
                scaffold(after, LangId::Typescript).as_ref(),
                "{after}"
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
            "wrap({ fn: [() => 1] })",
            "wrap({ fn: inner(() => 1) })",
            "wrap({ get fn() { return 1; } })",
            "wrap({ set fn(value) { this.value = value; } })",
            "wrap({ fn: class { method() { return 1; } } })",
            "wrap([() => 1])",
            "wrap([{ fn: () => 1 }])",
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
