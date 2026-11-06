//! 標準 format! と証明できる引数内だけ、外側のローカルな値束縛を使う。

use super::*;
use crate::engine::tree_walk::{Children, any_node};

#[derive(Clone, Default)]
pub(super) struct Environment {
    imports: HashSet<String>,
    attributes: HashSet<String>,
    used_macros: HashSet<String>,
}

impl Environment {
    fn merge(&mut self, other: Self) {
        self.imports.extend(other.imports);
        self.attributes.extend(other.attributes);
        self.used_macros.extend(other.used_macros);
    }

    fn plain(&self, macro_name: &str) -> bool {
        !self.imports.contains(macro_name)
            && self.imports.is_disjoint(&self.attributes)
            && self.imports.is_disjoint(&self.used_macros)
    }
}

fn environment(
    scope: Node<'_>,
    source: &[u8],
    approved: Option<usize>,
    imports_in_scope: bool,
) -> Option<Environment> {
    let mut result = Environment::default();
    let mut textual = HashSet::new();
    let mut glob = false;
    let mut cursor = scope.walk();
    for item in scope.named_children(&mut cursor) {
        match item.kind() {
            "use_declaration" => {
                let argument = item.child_by_field_name("argument")?;
                let mut cursor = argument.walk();
                let parent_glob = argument.kind() == "use_wildcard"
                    && argument
                        .named_children(&mut cursor)
                        .any(|node| node.kind() == "super")
                    && argument.named_children(&mut argument.walk()).all(|node| {
                        matches!(node.kind(), "super" | "line_comment" | "block_comment")
                    });
                if !parent_glob {
                    if let Some(names) = root_use_names(item, source) {
                        result.imports.extend(names);
                    } else {
                        glob = true;
                    }
                }
            }
            "extern_crate_declaration" => return None,
            "attribute_item" | "inner_attribute_item" => {
                if item
                    .named_child(0)
                    .and_then(|node| node.named_child(0))
                    .and_then(|node| node.utf8_text(source).ok())
                    .is_some_and(|name| matches!(name, "global_allocator" | "path"))
                {
                    continue;
                }
                result
                    .attributes
                    .extend(super::super::rust_std_origin::safe_attribute(item, source)?);
            }
            "macro_definition" => {
                let name = item.child_by_field_name("name")?.utf8_text(source).ok()?;
                let name = name.strip_prefix("r#").unwrap_or(name).to_owned();
                textual.insert(name.clone());
                result.imports.insert(name);
            }
            "macro_invocation" if Some(item.id()) != approved => {
                let name = item.child_by_field_name("macro")?.utf8_text(source).ok()?;
                let name = name.strip_prefix("r#").unwrap_or(name);
                if !is_allowed_expression_macro(name) {
                    return None;
                }
                result.used_macros.insert(name.to_owned());
            }
            "expression_statement" => {
                let mut cursor = item.walk();
                for node in item
                    .named_children(&mut cursor)
                    .filter(|node| node.kind() == "macro_invocation" && Some(node.id()) != approved)
                {
                    let name = node.child_by_field_name("macro")?.utf8_text(source).ok()?;
                    let name = name.strip_prefix("r#").unwrap_or(name);
                    if !is_allowed_expression_macro(name) {
                        return None;
                    }
                    result.used_macros.insert(name.to_owned());
                }
            }
            _ => {}
        }
    }
    // 親 module の use は子へ継承されないが、macro_rules! の字句スコープは届く。
    if !imports_in_scope && result.attributes.is_empty() && result.used_macros.is_empty() {
        result.imports = textual;
    } else if glob {
        return None;
    }
    Some(result)
}

fn sibling(mut node: Node<'_>, previous: bool) -> Option<Node<'_>> {
    loop {
        node = if previous {
            node.prev_sibling()?
        } else {
            node.next_sibling()?
        };
        if !matches!(node.kind(), "line_comment" | "block_comment") {
            return Some(node);
        }
    }
}

fn argument_macro<'t>(node: Node<'t>, source: &[u8]) -> Option<(Node<'t>, Node<'t>)> {
    if node.kind() != "identifier"
        || sibling(node, true).is_some_and(|node| matches!(node.kind(), "::" | "."))
        || sibling(node, false).is_none_or(|node| {
            node.kind() != "token_tree"
                || node.child(0).and_then(|node| node.utf8_text(source).ok()) != Some("(")
        })
    {
        return None;
    }
    let mut ancestor = node.parent();
    let mut invocation = None;
    while let Some(current) = ancestor {
        if current.kind() == "macro_invocation" {
            invocation = Some(current);
            break;
        }
        if current.kind() != "token_tree" {
            return None;
        }
        ancestor = current.parent();
    }
    let invocation = invocation?;
    let callee = invocation.child_by_field_name("macro")?;
    if callee.kind() != "identifier"
        || callee.utf8_text(source).ok()?.trim_start_matches("r#") != "format"
    {
        return None;
    }
    let mut cursor = invocation.walk();
    let body = invocation
        .named_children(&mut cursor)
        .find(|node| node.kind() == "token_tree")?;
    let mut outer = node;
    while outer
        .parent()
        .is_some_and(|parent| parent.id() != body.id())
    {
        outer = outer.parent()?;
    }
    let mut start = body.start_byte();
    let mut end = body.end_byte();
    let mut cursor = body.walk();
    for child in body
        .children(&mut cursor)
        .filter(|child| child.kind() == ",")
    {
        if child.end_byte() <= outer.start_byte() {
            start = child.end_byte();
        }
        if child.start_byte() >= outer.end_byte() {
            end = child.start_byte();
            break;
        }
    }
    // 別の引数の block は影響しない。対象の引数では束縛・別の展開を作る構文を保留する。
    let mut cursor = body.walk();
    if body
        .children(&mut cursor)
        .filter(|child| start <= child.start_byte() && child.end_byte() <= end)
        .any(|child| {
            any_node(child, Children::All, |node| {
                matches!(
                    node.kind(),
                    "{" | "!" | "|" | "||" | "let" | "if" | "while" | "for" | "match"
                )
            })
        })
    {
        return None;
    }
    Some((invocation, callee))
}

/// 暗黙キャプチャを含む書式マクロの呼び出しと、その呼び出し名。別のマクロの token tree の
/// 中の呼び出しと、パスで書いたマクロ名は扱わない。
fn capture_macro<'t, 's>(
    tree: Node<'t>,
    source: &'s [u8],
) -> Option<(Node<'t>, Node<'t>, &'s str)> {
    let invocation = tree
        .parent()
        .filter(|node| node.kind() == "macro_invocation")?;
    let callee = invocation.child_by_field_name("macro")?;
    if callee.kind() != "identifier" {
        return None;
    }
    let text = callee.utf8_text(source).ok()?;
    let name = text.strip_prefix("r#").unwrap_or(text);
    crate::engine::refs::is_capture_format_macro(name).then_some((invocation, callee, name))
}

impl RustPathOwnerIndex {
    pub(in crate::engine::impact) fn format_local_value_owner(
        &self,
        node: Node<'_>,
        source: &[u8],
        name: &str,
        dir: &Path,
        ref_path: &str,
        project: &RustPathOwnerProjectProof,
    ) -> bool {
        if !self.local_value_names.contains(name) || blake3::hash(source) != self.source_hash {
            return false;
        }
        let Some((invocation, callee)) = argument_macro(node, source) else {
            return false;
        };
        self.standard_macro_call(invocation, "format", source, dir, ref_path, project)
            && self.local_value_owner_beside(callee, source, name, Some(invocation.id()))
    }

    /// 書式の暗黙キャプチャが、参照ファイル直下の一意な値の宣言を指すか。キャプチャは呼び出し
    /// 位置の式の識別子と同じに解決されるので、呼び出し名のノードで同じ束縛の証明を使う。
    pub(in crate::engine::impact) fn format_capture_local_owner(
        &self,
        tree: Node<'_>,
        source: &[u8],
        name: &str,
        dir: &Path,
        ref_path: &str,
        project: &RustPathOwnerProjectProof,
    ) -> bool {
        if !self.local_value_names.contains(name) || blake3::hash(source) != self.source_hash {
            return false;
        }
        let Some((invocation, callee, macro_name)) = capture_macro(tree, source) else {
            return false;
        };
        self.standard_macro_call(invocation, macro_name, source, dir, ref_path, project)
            && self.local_value_owner_beside(callee, source, name, Some(invocation.id()))
    }

    /// 書式の暗黙キャプチャが、直下の crate 起点の import の束縛を使うか。
    pub(in crate::engine::impact) fn format_capture_import_binding_safe(
        &self,
        tree: Node<'_>,
        source: &[u8],
        name: &str,
        dir: &Path,
        ref_path: &str,
        project: &RustPathOwnerProjectProof,
    ) -> bool {
        let Some((invocation, callee, macro_name)) = capture_macro(tree, source) else {
            return false;
        };
        self.standard_macro_call(invocation, macro_name, source, dir, ref_path, project)
            && self.import_binding_safe_beside(callee, source, name, Some(invocation.id()))
    }

    /// `invocation` が標準の `macro_name!` を指すか。祖先のファイルと外側の block で、マクロ名を
    /// import・定義で差し替えておらず、ソースの配置と依存の置き換えも無いことを確かめる。
    fn standard_macro_call(
        &self,
        invocation: Node<'_>,
        macro_name: &str,
        source: &[u8],
        dir: &Path,
        ref_path: &str,
        project: &RustPathOwnerProjectProof,
    ) -> bool {
        let prove = || {
            let base = self
                .format_environment
                .get_or_init(|| {
                    if !project.allows_static_source_path(Path::new(ref_path)) {
                        return None;
                    }
                    let (package, _, _) = source_module_path(dir, Path::new(ref_path))?;
                    if !project
                        .cargo_configs
                        .keeps_dependency_sources(&dir.join(&package))
                    {
                        return None;
                    }
                    let mut combined = Environment::default();
                    for file in enum_owner::ancestor_files(dir, ref_path)? {
                        if !project.allows_static_source_path(&file) {
                            return None;
                        }
                        let source = read_input_file(&dir.join(&file))?;
                        let tree = parser::parse_source(&source, LangId::Rust).ok()?;
                        if tree.root_node().has_error() {
                            return None;
                        }
                        combined.merge(environment(
                            tree.root_node(),
                            &source,
                            None,
                            file == Path::new(ref_path),
                        )?);
                    }
                    // マクロ名ごとの判定は外側の block を合わせてから行う (`plain` は要素を
                    // 足すほど偽に寄るので、先に絞っても結果は変わらない)。
                    Some(combined)
                })
                .as_ref()?;
            let mut combined = base.clone();
            let mut ancestor = invocation.parent();
            while let Some(scope) = ancestor {
                if scope.kind() == "block" {
                    combined.merge(environment(scope, source, Some(invocation.id()), true)?);
                }
                ancestor = scope.parent();
            }
            combined.plain(macro_name).then_some(())
        };
        prove().is_some()
    }
}

pub(super) fn stock_expression(
    node: Node<'_>,
    source: &[u8],
    dir: &Path,
    ref_path: &str,
    project: &RustPathOwnerProjectProof,
    relocated: &[PathBuf],
) -> bool {
    let prove = || {
        let mut ancestor = node.parent();
        let mut invocation = None;
        while let Some(current) = ancestor {
            if current.kind() == "macro_invocation" {
                invocation = Some(current);
                break;
            }
            if current.kind() != "token_tree" {
                return None;
            }
            ancestor = current.parent();
        }
        let invocation = invocation?;
        let callee = invocation.child_by_field_name("macro")?;
        let macro_name = callee
            .utf8_text(source)
            .ok()?
            .strip_prefix("r#")
            .unwrap_or(callee.utf8_text(source).ok()?);
        if callee.kind() != "identifier"
            || !matches!(
                macro_name,
                "assert"
                    | "assert_eq"
                    | "assert_ne"
                    | "debug_assert"
                    | "debug_assert_eq"
                    | "debug_assert_ne"
                    | "matches"
                    | "format"
            )
        {
            return None;
        }
        let mut cursor = invocation.walk();
        let body = invocation
            .named_children(&mut cursor)
            .find(|node| node.kind() == "token_tree")?;
        if any_node(body, Children::All, |node| {
            node.kind() == "!"
                && sibling(node, true).is_some_and(|node| node.kind() == "identifier")
                && sibling(node, false).is_some_and(|node| node.kind() == "token_tree")
        }) {
            return None;
        }
        let (package, _, _) = source_module_path(dir, Path::new(ref_path))?;
        if !project
            .cargo_configs
            .keeps_dependency_sources(&dir.join(&package))
        {
            return None;
        }
        let manifest = read_input_text_file(&dir.join(&package).join("Cargo.toml"))?;
        let value = toml::from_str::<toml::Value>(&manifest).ok()?;
        if super::super::rust_std_origin::has_sysroot_dependency(&value, false) {
            return None;
        }
        let mut combined = Environment::default();
        for file in enum_owner::ancestor_files(dir, ref_path)? {
            let canonical = dir.join(&file).canonicalize().ok()?;
            if relocated.iter().any(|path| canonical.starts_with(path)) {
                return None;
            }
            let bytes = read_input_file(&dir.join(&file))?;
            if file == Path::new(ref_path) && bytes != source {
                return None;
            }
            let tree = parser::parse_source(&bytes, LangId::Rust).ok()?;
            if tree.root_node().has_error() {
                return None;
            }
            combined.merge(environment(tree.root_node(), &bytes, None, true)?);
        }
        let mut ancestor = invocation.parent();
        while let Some(scope) = ancestor {
            if scope.kind() == "block"
                || scope.kind() == "declaration_list"
                    && scope
                        .parent()
                        .is_some_and(|parent| parent.kind() == "mod_item")
            {
                combined.merge(environment(scope, source, Some(invocation.id()), true)?);
            }
            ancestor = scope.parent();
        }
        combined.plain(macro_name).then_some(())
    };
    prove().is_some()
}
