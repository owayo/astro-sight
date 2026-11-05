//! 宣言の型注釈が参照する型名が、旧新で同じものを指すかを確かめる。
//!
//! 型注釈のトークンが同じでも、参照する同じファイルの型別名や interface が同じ diff で
//! 変わると、型そのものは変わる。参照をたどれない形や、確かめられない名前は証明しない。

use std::collections::{BTreeSet, HashMap};

use tree_sitter::Node;

use super::*;
use crate::commands::dead_code_default_liveness::static_specifier;

/// 依存をたどる環境。
pub(crate) struct DependencyEnvironment<'a> {
    /// 宣言のあるファイル (dir 相対、`/` 区切り)。
    pub(crate) target_path: &'a str,
    /// この diff が変えたファイル (旧・新のパス)。
    pub(crate) changed_paths: &'a BTreeSet<String>,
    /// 宣言のあるファイルからの bare specifier が、registry から入れた依存を指すか。
    pub(crate) registry_dependency: &'a dyn Fn(&str) -> bool,
    /// 大域の名前を、リポジトリのどこかで宣言しているか (確かめられなければ true)。
    pub(crate) declared_globally: &'a dyn Fn(&str) -> bool,
}

/// `roots` は新の側の型ノード (旧の側とトークンが同じことは呼び出し側が確かめる)。
/// 同じファイルのトップレベルの型別名・interface を推移的にたどって旧新のトークン一致を
/// 求める。ほかのファイルの型は推移的にたどらない。相対 import の型は、宣言のファイルの別の
/// export を経由して戻ってくる依存もあり得るので証明しない。registry の依存から import した
/// 型と、リポジトリで宣言していない大域の型 (lib や @types のもの) は、diff が宣言のファイル
/// 以外を 1 つも変えていないときだけ受け入れる。`typeof`・`infer`・mapped type・値の式・
/// class や enum への依存・確かめられない名前は証明しない。
pub(crate) fn type_dependencies_unchanged(
    env: &DependencyEnvironment<'_>,
    old_root: Node<'_>,
    old_source: &[u8],
    new_root: Node<'_>,
    new_source: &[u8],
    roots: &[Node<'_>],
) -> bool {
    check(env, old_root, old_source, new_root, new_source, roots).is_some()
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Reference {
    pub(super) name: String,
    /// `NS.Member` の `Member`。
    pub(super) member: Option<String>,
}

enum ImportKind {
    Named(String),
    Default,
    Namespace,
}

enum Binding<'tree> {
    /// 型別名と interface (interface は merge するので複数あり得る)。`(文, 宣言)` の列。
    Types(Vec<(Node<'tree>, Node<'tree>)>),
    Import(Node<'tree>, ImportKind),
    /// class・enum・namespace・`import x = ...`・宣言の衝突など、中身をたどらない束縛。
    Other,
}

/// ファイルのトップレベルの型の名前空間の束縛と、export し得る名前。
struct TopLevel<'tree> {
    bindings: HashMap<String, Binding<'tree>>,
    /// import 以外で宣言した型の名前 (型別名・interface・class・enum・namespace)。
    declared: BTreeSet<String>,
    /// export 指定子が公開する名前。
    exported: BTreeSet<String>,
    default_export: bool,
}

impl<'tree> TopLevel<'tree> {
    fn collect(root: Node<'tree>, source: &[u8]) -> Option<Self> {
        let mut top = Self {
            bindings: HashMap::new(),
            declared: BTreeSet::new(),
            exported: BTreeSet::new(),
            default_export: false,
        };
        let mut cursor = root.walk();
        for statement in root.named_children(&mut cursor) {
            if statement.is_extra() {
                continue;
            }
            let declaration = if statement.kind() == "export_statement" {
                let mut cursor = statement.walk();
                top.default_export |= statement
                    .children(&mut cursor)
                    .any(|child| child.kind() == "default");
                let _: std::ops::ControlFlow<()> = crate::engine::tree_walk::walk_preorder(
                    statement,
                    crate::engine::tree_walk::Children::Named,
                    |node, _| {
                        if node.kind() == "export_specifier"
                            && let Some(exported) = node
                                .child_by_field_name("alias")
                                .or_else(|| node.child_by_field_name("name"))
                            && let Ok(text) = exported.utf8_text(source)
                        {
                            top.exported.insert(text.to_owned());
                        }
                        std::ops::ControlFlow::Continue(crate::engine::tree_walk::Walk::Descend)
                    },
                );
                match statement.child_by_field_name("declaration") {
                    Some(declaration) => declaration,
                    None => continue,
                }
            } else {
                statement
            };
            match declaration.kind() {
                "type_alias_declaration" | "interface_declaration" => {
                    let name = plain(declaration.child_by_field_name("name")?, source)?;
                    top.declared.insert(name.clone());
                    top.insert(name, Binding::Types(vec![(statement, declaration)]));
                }
                "class_declaration" | "abstract_class_declaration" | "enum_declaration" => {
                    let name = plain(declaration.child_by_field_name("name")?, source)?;
                    top.declared.insert(name.clone());
                    top.insert(name, Binding::Other);
                }
                // 宣言ファイル側の augmentation や `declare` は、ファイルの外の型も差し替え得る。
                "ambient_declaration" => return None,
                "expression_statement" | "internal_module" | "module" => {
                    let mut cursor = declaration.walk();
                    let modules: Vec<_> = if declaration.kind() == "expression_statement" {
                        declaration
                            .named_children(&mut cursor)
                            .filter(|child| child.kind() == "internal_module")
                            .collect()
                    } else {
                        vec![declaration]
                    };
                    for module in modules {
                        // namespace の名前の先頭だけが、このファイルの束縛になる。`declare module
                        // "x"` (文字列の名前) は束縛を作らない。
                        let name = module.child_by_field_name("name")?;
                        if name.kind() == "string" {
                            continue;
                        }
                        let head = plain(leftmost_identifier(name)?, source)?;
                        top.declared.insert(head.clone());
                        top.insert(head, Binding::Other);
                    }
                }
                "import_statement" => top.collect_import(declaration, source)?,
                "import_alias" => return None,
                _ => {}
            }
        }
        Some(top)
    }

    fn collect_import(&mut self, statement: Node<'tree>, source: &[u8]) -> Option<()> {
        let mut cursor = statement.walk();
        for child in statement.named_children(&mut cursor) {
            match child.kind() {
                "import_clause" => {
                    let mut cursor = child.walk();
                    for part in child.named_children(&mut cursor) {
                        match part.kind() {
                            "comment" => {}
                            "identifier" => {
                                self.insert(
                                    plain(part, source)?,
                                    Binding::Import(statement, ImportKind::Default),
                                );
                            }
                            "namespace_import" => {
                                let mut cursor = part.walk();
                                let local = part
                                    .named_children(&mut cursor)
                                    .find(|node| node.kind() == "identifier")?;
                                self.insert(
                                    plain(local, source)?,
                                    Binding::Import(statement, ImportKind::Namespace),
                                );
                            }
                            "named_imports" => {
                                let mut cursor = part.walk();
                                for spec in part.named_children(&mut cursor) {
                                    if spec.kind() == "comment" {
                                        continue;
                                    }
                                    if spec.kind() != "import_specifier" {
                                        return None;
                                    }
                                    let imported = spec.child_by_field_name("name")?;
                                    if imported.kind() != "identifier" {
                                        return None;
                                    }
                                    let local =
                                        spec.child_by_field_name("alias").unwrap_or(imported);
                                    self.insert(
                                        plain(local, source)?,
                                        Binding::Import(
                                            statement,
                                            ImportKind::Named(plain(imported, source)?),
                                        ),
                                    );
                                }
                            }
                            _ => return None,
                        }
                    }
                }
                "import_require_clause" => return None,
                _ => {}
            }
        }
        Some(())
    }

    fn insert(&mut self, name: String, binding: Binding<'tree>) {
        let interface = |declarations: &[(Node<'_>, Node<'_>)]| {
            declarations
                .iter()
                .all(|(_, declaration)| declaration.kind() == "interface_declaration")
        };
        match (self.bindings.get_mut(&name), binding) {
            (None, binding) => {
                self.bindings.insert(name, binding);
            }
            // merge するのは interface 同士だけ。型別名との重複は宣言の衝突。
            (Some(Binding::Types(existing)), Binding::Types(more))
                if interface(existing) && interface(&more) =>
            {
                existing.extend(more);
            }
            (Some(existing), _) => *existing = Binding::Other,
        }
    }

    /// `name` を export し得るか (型の宣言・export 指定子・default export)。import しただけの
    /// 名前は、export 指定子で公開しない限り export しない。
    fn may_export(&self, name: &str) -> bool {
        self.declared.contains(name)
            || self.exported.contains(name)
            || (name == "default" && self.default_export)
    }
}

fn plain(node: Node<'_>, source: &[u8]) -> Option<String> {
    let text = node.utf8_text(source).ok()?;
    (!text.is_empty() && !text.contains('\\')).then(|| text.to_owned())
}

/// `A.B.C` の先頭の識別子。
fn leftmost_identifier(mut node: Node<'_>) -> Option<Node<'_>> {
    loop {
        match node.kind() {
            "identifier" => return Some(node),
            "nested_identifier" | "member_expression" => {
                node = node.child_by_field_name("object")?;
            }
            _ => return None,
        }
    }
}

/// `nested_type_identifier` (`NS.Member` / `NS.A.B`) の先頭の名前と、その直後の名前。
fn nested_root(node: Node<'_>, source: &[u8]) -> Option<(String, String)> {
    let mut module = node.child_by_field_name("module")?;
    let mut member = node.child_by_field_name("name")?;
    while module.kind() == "nested_identifier" {
        member = module.child_by_field_name("property")?;
        module = module.child_by_field_name("object")?;
    }
    if module.kind() != "identifier" {
        return None;
    }
    Some((plain(module, source)?, plain(member, source)?))
}

/// 名前の子を持たず、子をそのまま型としてたどる種類。
const TRANSPARENT_TYPE_KINDS: &[&str] = &[
    "type_annotation",
    "opting_type_annotation",
    "omitting_type_annotation",
    "adding_type_annotation",
    "asserts_annotation",
    "type_predicate_annotation",
    "predefined_type",
    "this_type",
    "generic_type",
    "type_arguments",
    "object_type",
    "interface_body",
    "extends_type_clause",
    "call_signature",
    "construct_signature",
    "union_type",
    "intersection_type",
    "array_type",
    "tuple_type",
    "readonly_type",
    "parenthesized_type",
    "function_type",
    "constructor_type",
    "formal_parameters",
    "literal_type",
    "unary_expression",
    "string",
    "string_fragment",
    "escape_sequence",
    "number",
    "true",
    "false",
    "null",
    "undefined",
    "template_literal_type",
    "template_type",
    "conditional_type",
    "lookup_type",
    "index_type_query",
    "type_parameters",
    "constraint",
    "default_type",
    "optional_type",
    "rest_type",
];

/// 型の部分木から、型パラメータに束縛されない型名の参照を集める。たどれない形は None。
fn collect_references(root: Node<'_>, source: &[u8], out: &mut Vec<Reference>) -> Option<()> {
    collect_references_in_scope(root, source, out, &[])
}

pub(super) fn collect_references_in_scope(
    root: Node<'_>,
    source: &[u8],
    out: &mut Vec<Reference>,
    outer_names: &[String],
) -> Option<()> {
    // 型パラメータの scope。`(親, 名前)` の列で、0 番は空の外側。
    let mut scopes: Vec<(Option<usize>, Vec<String>)> = vec![(None, outer_names.to_vec())];
    let mut stack = vec![(root, 0usize)];
    while let Some((node, outer)) = stack.pop() {
        if node.is_extra() {
            continue;
        }
        let scope = match node.child_by_field_name("type_parameters") {
            Some(parameters) => {
                let mut names = Vec::new();
                let mut cursor = parameters.walk();
                for parameter in parameters.named_children(&mut cursor) {
                    if parameter.is_extra() {
                        continue;
                    }
                    if parameter.kind() != "type_parameter" {
                        return None;
                    }
                    names.push(plain(parameter.child_by_field_name("name")?, source)?);
                }
                scopes.push((Some(outer), names));
                scopes.len() - 1
            }
            None => outer,
        };
        let mut children: Vec<Node<'_>> = Vec::new();
        match node.kind() {
            "type_identifier" => {
                let name = plain(node, source)?;
                if !bound(&scopes, scope, &name) {
                    out.push(Reference { name, member: None });
                }
            }
            "nested_type_identifier" => {
                let (name, member) = nested_root(node, source)?;
                if !bound(&scopes, scope, &name) {
                    out.push(Reference {
                        name,
                        member: Some(member),
                    });
                }
            }
            // 引数名・添字の名前・型述語の対象は束縛で、型の参照ではない。
            "required_parameter" | "optional_parameter" => {
                children.extend(node.child_by_field_name("type"));
            }
            "type_predicate" => children.extend(node.child_by_field_name("type")),
            "asserts" => {
                let mut cursor = node.walk();
                children.extend(
                    node.named_children(&mut cursor)
                        .filter(|child| child.kind() == "type_predicate"),
                );
            }
            "index_signature" => {
                let name = node.child_by_field_name("name");
                let mut cursor = node.walk();
                children.extend(
                    node.named_children(&mut cursor)
                        .filter(|child| Some(child.id()) != name.map(|name| name.id())),
                );
            }
            "property_signature" | "method_signature" => {
                let name = node.child_by_field_name("name")?;
                if !matches!(name.kind(), "property_identifier" | "string" | "number") {
                    return None;
                }
                let mut cursor = node.walk();
                children.extend(
                    node.named_children(&mut cursor)
                        .filter(|child| child.id() != name.id()),
                );
            }
            "type_parameter" => {
                children.extend(node.child_by_field_name("constraint"));
                children.extend(node.child_by_field_name("value"));
            }
            "type_alias_declaration" | "interface_declaration" => {
                let name = node.child_by_field_name("name")?;
                let mut cursor = node.walk();
                children.extend(
                    node.named_children(&mut cursor)
                        .filter(|child| child.id() != name.id()),
                );
            }
            kind if TRANSPARENT_TYPE_KINDS.contains(&kind) => {
                let mut cursor = node.walk();
                children.extend(node.named_children(&mut cursor));
            }
            _ => return None,
        }
        stack.extend(children.into_iter().rev().map(|child| (child, scope)));
    }
    Some(())
}

fn bound(scopes: &[(Option<usize>, Vec<String>)], mut scope: usize, name: &str) -> bool {
    loop {
        let (parent, names) = &scopes[scope];
        if names.iter().any(|bound| bound == name) {
            return true;
        }
        match parent {
            Some(parent) => scope = *parent,
            None => return false,
        }
    }
}

fn check(
    env: &DependencyEnvironment<'_>,
    old_root: Node<'_>,
    old_source: &[u8],
    new_root: Node<'_>,
    new_source: &[u8],
    roots: &[Node<'_>],
) -> Option<()> {
    let mut pending = Vec::new();
    for root in roots {
        collect_references(*root, new_source, &mut pending)?;
    }
    check_references(env, old_root, old_source, new_root, new_source, pending)
}

pub(super) fn references_unchanged(
    env: &DependencyEnvironment<'_>,
    old_root: Node<'_>,
    old_source: &[u8],
    new_root: Node<'_>,
    new_source: &[u8],
    references: Vec<Reference>,
) -> bool {
    check_references(env, old_root, old_source, new_root, new_source, references).is_some()
}

fn check_references(
    env: &DependencyEnvironment<'_>,
    old_root: Node<'_>,
    old_source: &[u8],
    new_root: Node<'_>,
    new_source: &[u8],
    mut pending: Vec<Reference>,
) -> Option<()> {
    let old = TopLevel::collect(old_root, old_source)?;
    let new = TopLevel::collect(new_root, new_source)?;
    let isolated = env.changed_paths.iter().all(|path| path == env.target_path);
    let mut seen = BTreeSet::new();
    while let Some(reference) = pending.pop() {
        if !seen.insert(reference.clone()) {
            continue;
        }
        match (
            old.bindings.get(&reference.name),
            new.bindings.get(&reference.name),
        ) {
            // 大域の型 (`Record`・`JSX` など)。lib や @types のものだけを受け入れ、リポジトリで
            // 宣言した大域の型 (`declare global` や module でない .d.ts) は証明しない。
            (None, None) => {
                if !isolated || (env.declared_globally)(&reference.name) {
                    return None;
                }
            }
            (Some(Binding::Types(old_declarations)), Some(Binding::Types(new_declarations))) => {
                if reference.member.is_some() || old_declarations.len() != new_declarations.len() {
                    return None;
                }
                for ((old_statement, _), (new_statement, new_declaration)) in
                    old_declarations.iter().zip(new_declarations)
                {
                    if node_signature_tokens(*old_statement, old_source)?
                        != node_signature_tokens(*new_statement, new_source)?
                    {
                        return None;
                    }
                    collect_references(*new_declaration, new_source, &mut pending)?;
                }
            }
            (
                Some(Binding::Import(old_statement, _)),
                Some(Binding::Import(new_statement, kind)),
            ) => {
                // 解決先のファイルが変わらなくても、その先の再 export や依存の変更で型は
                // 変わり得る。import した型は、ほかのファイルを変えていない diff だけで受け入れる。
                if !isolated
                    || node_signature_tokens(*old_statement, old_source)?
                        != node_signature_tokens(*new_statement, new_source)?
                {
                    return None;
                }
                // 相対 import の型は、宣言のファイルの別の export を経由して戻ってくる依存を含め、
                // 推移的にたどらないので証明しない。bare specifier は registry の依存だけを信頼する。
                let specifier =
                    static_specifier(new_statement.child_by_field_name("source")?, new_source)?;
                if specifier.starts_with(['.', '/'].as_slice())
                    || !(env.registry_dependency)(specifier)
                {
                    return None;
                }
                // 宣言のファイル自身が export し得る名前は、別名の解決で戻ってくることがある。
                let exported = match kind {
                    ImportKind::Named(imported) => imported.as_str(),
                    ImportKind::Default => "default",
                    ImportKind::Namespace => reference.member.as_deref()?,
                };
                if old.may_export(exported) || new.may_export(exported) {
                    return None;
                }
            }
            _ => return None,
        }
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::parser;
    use crate::language::LangId;

    /// `Props` 型の右辺 (旧新でトークンが同じ) を起点に依存を確かめる。
    /// `registry` は registry の依存とみなす bare specifier。
    fn unchanged(old: &str, new: &str, changed: &[&str], registry: &[&str]) -> bool {
        unchanged_in(old, new, changed, registry, &[])
    }

    /// `globals` はリポジトリで宣言した大域の名前。
    fn unchanged_in(
        old: &str,
        new: &str,
        changed: &[&str],
        registry: &[&str],
        globals: &[&str],
    ) -> bool {
        let lang = LangId::Tsx;
        let old_tree = parser::parse_source(old.as_bytes(), lang).unwrap();
        let new_tree = parser::parse_source(new.as_bytes(), lang).unwrap();
        let root = new_tree.root_node();
        let mut cursor = root.walk();
        let props = root
            .named_children(&mut cursor)
            .find_map(|statement| {
                let declaration = statement
                    .child_by_field_name("declaration")
                    .unwrap_or(statement);
                (declaration.kind() == "function_declaration").then(|| {
                    declaration
                        .child_by_field_name("parameters")
                        .unwrap()
                        .named_child(0)
                        .unwrap()
                        .child_by_field_name("type")
                        .unwrap()
                })
            })
            .unwrap();
        let changed: BTreeSet<String> = changed.iter().map(|path| path.to_string()).collect();
        let registry_dependency = |specifier: &str| registry.contains(&specifier);
        let declared_globally = |name: &str| globals.contains(&name);
        let env = DependencyEnvironment {
            target_path: "panel.tsx",
            changed_paths: &changed,
            registry_dependency: &registry_dependency,
            declared_globally: &declared_globally,
        };
        type_dependencies_unchanged(
            &env,
            old_tree.root_node(),
            old.as_bytes(),
            root,
            new.as_bytes(),
            &[props],
        )
    }

    const PANEL: &str = "export function Panel(props: { opts: Opts }) { return null; }\n";

    #[test]
    fn type_dependencies_follow_same_file_declarations_transitively() {
        let old =
            format!("type Inner = {{ a: number }};\ntype Opts = {{ inner: Inner }};\n{PANEL}");
        assert!(unchanged(&old, &old, &["panel.tsx"], &[]));
        // 修飾子・省略可能・リテラル・合併などの型の構文は、そのままたどる。
        let shapes = format!(
            "type Opts = {{ readonly a?: 'x' | -1 | null; b: Inner[]; c: [Inner, ...string[]]; d: keyof Inner; e: Inner['a'] }};\ninterface Inner {{ a: number; m(x: string): void }}\n{PANEL}"
        );
        assert!(unchanged(&shapes, &shapes, &["panel.tsx", "other.ts"], &[]));
        for new in [
            old.replace("a: number", "a: string"),
            old.replace("type Opts = {", "type Opts<T = 1> = {"),
            old.replace("type Inner", "export type Inner"),
            format!("interface Inner {{ a: number }}\ntype Opts = {{ inner: Inner }};\n{PANEL}"),
        ] {
            assert!(!unchanged(&old, &new, &["panel.tsx"], &[]), "{new}");
        }
        // 削除したメンバーからしか届かない型は比べない。
        let old = format!("type Gone = {{ a: number }};\ntype Opts = {{ b: string }};\n{PANEL}");
        let new = old.replace("type Gone = { a: number };\n", "");
        assert!(unchanged(&old, &new, &["panel.tsx"], &[]));
    }

    #[test]
    fn type_dependencies_bind_type_parameters_by_scope() {
        let old = format!(
            "type Opts = {{ render: <T>(item: T) => T; map: Box<string> }};\ninterface Box<U> {{ value: U }}\n{PANEL}"
        );
        assert!(unchanged(&old, &old, &["panel.tsx", "other.ts"], &[]));
        // 型パラメータの外の同名の参照は、大域の型として扱う。
        let outside = old.replace("map: Box<string>", "map: T");
        assert!(!unchanged(
            &outside,
            &outside,
            &["panel.tsx", "other.ts"],
            &[]
        ));
        assert!(unchanged(&outside, &outside, &["panel.tsx"], &[]));
    }

    #[test]
    fn type_dependencies_check_imports_by_specifier_and_isolation() {
        let source = format!(
            "import type {{ ReactNode }} from 'react';\n{}",
            PANEL.replace("opts: Opts", "opts: ReactNode")
        );
        assert!(unchanged(&source, &source, &["panel.tsx"], &["react"]));
        // registry の依存の型でも、ほかのファイル (lock ファイルや package.json を含む) を
        // 変えた diff では証明しない。registry の依存と決められない指定子も証明しない。
        for changed in [
            &["panel.tsx", "types.ts"][..],
            &["panel.tsx", "package.json"][..],
        ] {
            assert!(
                !unchanged(&source, &source, changed, &["react"]),
                "{changed:?}"
            );
        }
        assert!(!unchanged(&source, &source, &["panel.tsx"], &[]));
        // 相対 import の型は、ほかのファイルを変えていない diff でも証明しない。
        let relative = format!("import type {{ Opts }} from './types';\n{PANEL}");
        assert!(!unchanged(
            &relative,
            &relative,
            &["panel.tsx"],
            &["./types"]
        ));
        // import 文の変更は同じ型を指す証明にならない。
        let moved = source.replace("'react'", "'react-dom'");
        assert!(!unchanged(
            &source,
            &moved,
            &["panel.tsx"],
            &["react", "react-dom"]
        ));
        // 自分自身を別名で import し得る名前は証明しない。
        let self_alias = format!(
            "import type {{ Opts }} from '@/panel';\nexport type {{ Local as Opts }};\ntype Local = {{ a: 1 }};\n{PANEL}"
        );
        assert!(!unchanged(
            &self_alias,
            &self_alias,
            &["panel.tsx"],
            &["@/panel"]
        ));
        let namespace = format!(
            "import * as UI from 'ui';\n{}",
            PANEL.replace("opts: Opts", "opts: UI.Opts")
        );
        assert!(unchanged(&namespace, &namespace, &["panel.tsx"], &["ui"]));
        // 大域の型は、リポジトリで宣言していないもの (lib や @types) だけを受け入れる。
        let global = PANEL.replace("opts: Opts", "opts: PanelOptions");
        assert!(unchanged(&global, &global, &["panel.tsx"], &[]));
        assert!(!unchanged_in(
            &global,
            &global,
            &["panel.tsx"],
            &[],
            &["PanelOptions"]
        ));
        assert!(!unchanged(
            &global,
            &global,
            &["panel.tsx", "other.ts"],
            &[]
        ));
    }

    #[test]
    fn type_dependencies_reject_untraceable_forms() {
        for declaration in [
            "type Opts = typeof value;",
            "type Opts = import('./x').T;",
            "type Opts = { [K in 'a']: K };",
            "type Opts = Box extends Array<infer V> ? V : never;",
            "type Opts = { [key]: string };",
            "class Opts {}",
            "enum Opts { A }",
            "namespace Opts { export type T = 1; }",
            "declare global { interface Window { x: 1 } }\ntype Opts = { a: 1 };",
            "import Opts = Other.Opts;",
            "type Opts = { a: 1 };\ninterface Opts { b: 2 }",
        ] {
            let source = format!("{declaration}\n{PANEL}");
            assert!(
                !unchanged(&source, &source, &["panel.tsx"], &[]),
                "{declaration}"
            );
        }
        // 同じ名前の interface の merge は、宣言の列が旧新で一致すればよい。
        let merged = format!("interface Opts {{ a: 1 }}\ninterface Opts {{ b: 2 }}\n{PANEL}");
        assert!(unchanged(&merged, &merged, &["panel.tsx"], &[]));
    }

    #[test]
    fn type_dependency_node_kinds_exist_in_both_grammars() {
        for lang in [LangId::Typescript, LangId::Tsx] {
            let grammar = lang.ts_language();
            for kind in TRANSPARENT_TYPE_KINDS.iter().chain(&[
                "type_identifier",
                "nested_type_identifier",
                "nested_identifier",
                "required_parameter",
                "optional_parameter",
                "type_predicate",
                "asserts",
                "index_signature",
                "property_signature",
                "method_signature",
                "type_parameter",
                "type_alias_declaration",
                "interface_declaration",
                "class_declaration",
                "abstract_class_declaration",
                "enum_declaration",
                "ambient_declaration",
                "expression_statement",
                "internal_module",
                "module",
                "import_statement",
                "import_clause",
                "namespace_import",
                "named_imports",
                "import_specifier",
                "import_require_clause",
                "import_alias",
                "export_statement",
                "export_specifier",
                "type_query",
                "infer_type",
                "mapped_type_clause",
                "computed_property_name",
            ]) {
                tree_sitter::Query::new(&grammar, &format!("({kind}) @node"))
                    .unwrap_or_else(|error| panic!("{lang:?}: {kind}: {error}"));
            }
        }
    }
}
