//! `$crate` から始まるマクロの固定パスと、そのクレートに宣言された関数。
use super::*;

enum Declaration {
    Function,
    InlineModule(usize),
    Unknown,
}

pub(super) struct Functions(Vec<HashMap<String, Declaration>>);
pub(super) type FunctionFiles = Mutex<HashMap<PathBuf, Arc<OnceLock<Option<Vec<Functions>>>>>>;

fn plain_scope(scope: Node<'_>, source: &[u8]) -> bool {
    !scope.has_error()
        && scope.named_children(&mut scope.walk()).all(|item| {
            item.kind() != "inner_attribute_item"
                || (enum_owner::attribute_name(item, source) != Some("cfg")
                    && super::super::rust_std_origin::safe_attribute(item, source)
                        .is_some_and(|names| names.is_empty()))
        })
}

/// 無条件の明示宣言だけを記録する。use や const に束縛された値は関数と断定しない。
impl Functions {
    fn parse(source: &[u8]) -> Option<Self> {
        let tree = parser::parse_source(source, LangId::Rust).ok()?;
        let mut scopes = vec![HashMap::new()];
        let mut pending = vec![(tree.root_node(), 0)];
        while let Some((scope, index)) = pending.pop() {
            if !plain_scope(scope, source) {
                continue;
            }
            let mut attributed = false;
            for item in scope.named_children(&mut scope.walk()) {
                match item.kind() {
                    "line_comment" | "block_comment" => continue,
                    "attribute_item" => {
                        attributed = true;
                        continue;
                    }
                    "function_item" | "mod_item" => {
                        let name = item
                            .child_by_field_name("name")?
                            .utf8_text(source)
                            .ok()?
                            .trim_start_matches("r#");
                        let declaration = if attributed || scopes[index].contains_key(name) {
                            Declaration::Unknown
                        } else if item.kind() == "function_item"
                            && item
                                .child_by_field_name("body")
                                .is_some_and(|body| plain_scope(body, source))
                        {
                            Declaration::Function
                        } else if item.kind() == "mod_item"
                            && let Some(body) = item.child_by_field_name("body")
                        {
                            let child = scopes.len();
                            scopes.push(HashMap::new());
                            pending.push((body, child));
                            Declaration::InlineModule(child)
                        } else {
                            Declaration::Unknown
                        };
                        scopes[index].insert(name.to_owned(), declaration);
                    }
                    _ => {}
                }
                attributed = false;
            }
        }
        Some(Self(scopes))
    }

    fn declares(&self, path: &[String]) -> bool {
        let mut index = 0;
        for (position, name) in path.iter().enumerate() {
            match self.0[index].get(name) {
                Some(Declaration::Function) => return position + 1 == path.len(),
                Some(Declaration::InlineModule(child)) => index = *child,
                _ => return false,
            }
        }
        false
    }
}

impl RustPathOwnerProjectProof {
    fn declares_macro_function(&self, file: &Path, path: &[String]) -> Option<bool> {
        let cell = self
            .macro_functions
            .lock()
            .expect("macro function cache poisoned")
            .entry(file.to_owned())
            .or_default()
            .clone();
        cell.get_or_init(|| {
            let bytes = read_input_file(&self.root.join(file))?;
            let mut versions = vec![Functions::parse(&bytes)?];
            if let Some(old) = self.reexport_old_sources.get(file) {
                versions.push(Functions::parse(old.as_deref()?)?);
            }
            Some(versions)
        })
        .as_ref()
        .map(|versions| versions.iter().all(|functions| functions.declares(path)))
    }

    pub(in crate::engine::impact) fn macro_function(
        &self,
        file: &Path,
        path: &[String],
    ) -> Option<PathBuf> {
        if !self.callable_layout_stable() || !self.unrelocated(file) {
            return None;
        }
        let module = self.ordinary_private_module(file)?;
        if !module.library_only
            || self
                .declared_module_file(&module.src, &module.path)
                .as_deref()
                != Some(file)
        {
            return None;
        }
        let stable = |path: &[String]| {
            super::super::rust_reexport_owner::stable_module_chain(
                &self.root,
                &module.src,
                path,
                self,
            )
        };
        stable(&module.path)?;
        for count in 0..path.len() {
            let owner = if count == 0 {
                module.src.join("lib.rs")
            } else {
                stable(&path[..count])?;
                self.declared_module_file(&module.src, &path[..count])?
            };
            if self.declares_macro_function(&owner, &path[count..])? {
                return self.unrelocated(&owner).then_some(owner);
            }
        }
        None
    }
}

impl RustPathOwnerIndex {
    pub(in crate::engine::impact) fn definition_crate_path(
        &self,
        node: Node<'_>,
        source: &[u8],
    ) -> Option<&[String]> {
        let paths = self.macro_paths.get_or_init(|| {
            let mut root = node;
            while let Some(parent) = root.parent() {
                root = parent;
            }
            definition_paths(root, source)
        });
        paths.get(&node.id()).map(Vec::as_slice)
    }
}

/// マクロや属性に渡す入力、生成する定義には入らず、通常のモジュールのマクロ定義だけを調べる。
fn definition_paths(root: Node<'_>, source: &[u8]) -> HashMap<usize, Vec<String>> {
    let mut result = HashMap::new();
    let mut scopes = vec![root];
    while let Some(scope) = scopes.pop() {
        if !plain_scope(scope, source) {
            continue;
        }
        let mut opaque = false;
        for item in scope.named_children(&mut scope.walk()) {
            match item.kind() {
                "line_comment" | "block_comment" => continue,
                "attribute_item" => {
                    opaque |= item.utf8_text(source).ok() != Some("#[macro_export]")
                        && !super::super::rust_std_origin::safe_attribute(item, source)
                            .is_some_and(|names| names.is_empty());
                    continue;
                }
                "mod_item" if !opaque => scopes.extend(item.child_by_field_name("body")),
                "macro_definition" if !opaque => {
                    for rule in item
                        .named_children(&mut item.walk())
                        .filter(|i| i.kind() == "macro_rule")
                    {
                        if let Some(body) = rule.child_by_field_name("right") {
                            record_rule_paths(body, source, &mut result);
                        }
                    }
                }
                _ => {}
            }
            opaque = false;
        }
    }
    result
}

fn group(node: Node<'_>, open: &str) -> bool {
    node.kind() == "token_tree" && node.child(0).is_some_and(|n| n.kind() == open)
}

/// 文として直接評価するパス、またはその呼び出しだけを記録する。
fn record_rule_paths(body: Node<'_>, source: &[u8], paths: &mut HashMap<usize, Vec<String>>) {
    let mut pending = vec![body];
    while let Some(tree) = pending.pop() {
        let tokens: Vec<_> = tree
            .children(&mut tree.walk())
            .skip(1)
            .take(tree.child_count().saturating_sub(2) as usize)
            .filter(|n| !matches!(n.kind(), "line_comment" | "block_comment"))
            .collect();
        for statement in tokens.split(|n| n.kind() == ";") {
            let Some((&first, rest)) = statement.split_first() else {
                continue;
            };
            if group(first, "(") || group(first, "{") {
                if rest.is_empty() || matches!(rest, [args] if group(*args, "(")) {
                    pending.push(first);
                }
            } else if first.kind() == "metavariable"
                && first.utf8_text(source).ok() == Some("$crate")
                && let Some((id, path)) = function_path(rest, source)
            {
                paths.insert(id, path);
            }
        }
    }
}

fn function_path(mut tokens: &[Node<'_>], source: &[u8]) -> Option<(usize, Vec<String>)> {
    let mut path = Vec::new();
    let mut last = None;
    while let [sep, name, rest @ ..] = tokens {
        if sep.kind() != "::" || name.kind() != "identifier" {
            return None;
        }
        let segment = name.utf8_text(source).ok()?.trim_start_matches("r#");
        if !segment.is_ascii() {
            return None;
        }
        path.push(segment.to_owned());
        last = Some(name.id());
        tokens = rest;
    }
    (tokens.is_empty() || matches!(tokens, [args] if group(*args, "("))).then_some((last?, path))
}
