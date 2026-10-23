use std::collections::HashMap;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use crate::engine::parser;
use crate::engine::tree_walk::{Children, Walk, walk_preorder};
use crate::git_support::normalize_workspace_separators;
use crate::language::LangId;

/// Ruby の静的な `Owner.new` から、通常の initialize への潜在的な利用だけを加算する。
/// 未証明な場合に Dead を返したり、従来の同名候補スキップを緩めたりはしない。
#[derive(Debug, Default)]
pub(crate) struct RubyConstructorLiveness {
    counts: HashMap<(String, String), (usize, usize)>,
}

impl RubyConstructorLiveness {
    pub(crate) fn build<F>(
        candidates: &[(String, String, String, LangId)],
        canonical_dir: &Path,
        files: &[PathBuf],
        is_test_path: F,
    ) -> Self
    where
        F: Fn(&Path) -> bool,
    {
        let mut candidate_counts = HashMap::<(String, String), usize>::new();
        for (name, kind, file, lang) in candidates {
            if *lang == LangId::Ruby
                && matches!(kind.as_str(), "function" | "method")
                && name.ends_with(".initialize")
            {
                *candidate_counts
                    .entry((file.clone(), name.clone()))
                    .or_default() += 1;
            }
        }
        if candidate_counts.is_empty() {
            return Self::default();
        }
        let Some(inventory) = Inventory::load(canonical_dir, files, &is_test_path) else {
            return Self::default();
        };
        inventory.resolve(&candidate_counts)
    }

    pub(crate) fn counts_for(&self, file: &str, name: &str) -> (usize, usize) {
        self.counts
            .get(&(file.to_string(), name.to_string()))
            .copied()
            .unwrap_or_default()
    }
}

const MAX_FILES: usize = 10_000;
const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;
const MAX_NODES: usize = 1_000_000;
const MAX_DEPTH: usize = 256;
const MAX_FACTS: usize = 100_000;
const MAX_FACT_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy)]
enum Binding {
    Module,
    Class(usize),
    Unknown,
}

struct Class {
    file: String,
    public_name: String,
    initializer_end: Option<usize>,
    supported: bool,
}

struct ConstructorCall {
    file: String,
    receiver: ConstantPath,
    lexical: String,
    start: usize,
    deferred: bool,
    test: bool,
}

struct ConstantPath {
    parts: Vec<String>,
    absolute: bool,
}

#[derive(Default)]
struct Inventory {
    bindings: HashMap<String, Binding>,
    classes: Vec<Class>,
    calls: Vec<ConstructorCall>,
    /// 公開候補が省略する module 名による衝突も、候補外の宣言を含めて数える。
    public_declarations: HashMap<(String, String), usize>,
    fact_bytes: usize,
}

impl Inventory {
    fn load<F>(canonical_dir: &Path, files: &[PathBuf], is_test_path: &F) -> Option<Self>
    where
        F: Fn(&Path) -> bool,
    {
        if files.len() > MAX_FILES {
            return None;
        }
        let mut inventory = Self::default();
        let mut total_bytes = 0usize;
        for file in files {
            let utf8 = camino::Utf8Path::new(file.to_str()?);
            let known_lang = LangId::from_path(utf8).ok();
            if known_lang.is_some_and(|lang| lang != LangId::Ruby)
                || (known_lang.is_none() && file.extension().is_some())
            {
                continue;
            }
            let relative = normalize_workspace_separators(
                &file.strip_prefix(canonical_dir).ok()?.to_string_lossy(),
            );
            if std::fs::metadata(file).ok()?.len() > MAX_FILE_BYTES as u64 {
                return None;
            }
            let source = parser::read_file(utf8).ok()?;
            let lang = known_lang.or_else(|| LangId::from_shebang_source(&source));
            if lang != Some(LangId::Ruby) {
                continue;
            }
            total_bytes = total_bytes.checked_add(source.len())?;
            if source.len() > MAX_FILE_BYTES || total_bytes > MAX_TOTAL_BYTES {
                return None;
            }
            let tree = parser::parse_source(&source, LangId::Ruby).ok()?;
            if tree.root_node().has_error() {
                return None;
            }
            inventory.collect(tree.root_node(), &source, &relative, is_test_path(file))?;
        }
        Some(inventory)
    }

    fn bind(&mut self, name: String, binding: Binding) -> Option<()> {
        self.fact_bytes = self.fact_bytes.saturating_add(name.len().saturating_mul(2));
        if self.fact_bytes > MAX_FACT_BYTES || self.bindings.len() >= MAX_FACTS {
            return None;
        }
        self.bindings
            .entry(name)
            .and_modify(|existing| {
                if !matches!((*existing, binding), (Binding::Module, Binding::Module)) {
                    *existing = Binding::Unknown;
                }
            })
            .or_insert(binding);
        Some(())
    }

    fn collect(
        &mut self,
        root: tree_sitter::Node<'_>,
        source: &[u8],
        file: &str,
        test: bool,
    ) -> Option<()> {
        // 深さを記録し、兄弟に移った時点で字句名前空間を戻す。
        let mut scopes = Vec::<(usize, String, bool)>::new();
        let mut deferred = Vec::<(usize, usize)>::new();
        let mut unevaluated = Vec::<usize>::new();
        let mut node_count = 0usize;
        let result = walk_preorder(root, Children::Named, |node, depth| {
            node_count += 1;
            if depth > MAX_DEPTH
                || node_count > MAX_NODES
                || self.classes.len() + self.calls.len() + self.bindings.len() > MAX_FACTS
                || node.is_missing()
            {
                return ControlFlow::Break(());
            }
            while scopes.last().is_some_and(|(d, _, _)| *d >= depth) {
                scopes.pop();
            }
            while deferred.last().is_some_and(|(d, _)| *d >= depth) {
                deferred.pop();
            }
            while unevaluated.last().is_some_and(|d| *d >= depth) {
                unevaluated.pop();
            }
            let namespace = scopes.last().map_or("", |(_, name, _)| name.as_str());
            match node.kind() {
                "class" | "module" => {
                    let Some(name) = node.child_by_field_name("name") else {
                        return ControlFlow::Break(());
                    };
                    // 修飾宣言は定数の所属と Module.nesting が一致しない。
                    if name.kind() != "constant" || !direct_namespace_item(node) {
                        return ControlFlow::Break(());
                    }
                    let Some(bare) = text(name, source) else {
                        return ControlFlow::Break(());
                    };
                    if !bare.is_ascii()
                        || (namespace.is_empty()
                            && matches!(
                                bare,
                                "Object" | "BasicObject" | "Class" | "Module" | "Struct"
                            ))
                    {
                        return ControlFlow::Break(());
                    }
                    let full = qualify(namespace, bare);
                    let has_parent = node.child_by_field_name("superclass").is_some();
                    let supported = !has_parent && scopes.iter().all(|(_, _, safe)| *safe);
                    if node.kind() == "class" {
                        let public_name = format!("{bare}.initialize");
                        *self
                            .public_declarations
                            .entry((file.to_string(), public_name.clone()))
                            .or_default() += 1;
                        let initializer_end = direct_initializer(node, source);
                        let index = self.classes.len();
                        self.classes.push(Class {
                            file: file.to_string(),
                            public_name,
                            initializer_end,
                            supported,
                        });
                        if self.bind(full.clone(), Binding::Class(index)).is_none() {
                            return ControlFlow::Break(());
                        }
                    } else {
                        if self.bind(full.clone(), Binding::Module).is_none() {
                            return ControlFlow::Break(());
                        }
                    }
                    scopes.push((depth, full, supported));
                }
                "method" | "singleton_method" => {
                    if let Some(name) = node.child_by_field_name("name") {
                        if text(name, source).is_some_and(|name| {
                            matches!(
                                name,
                                "new"
                                    | "const_missing"
                                    | "method_missing"
                                    | "inherited"
                                    | "included"
                                    | "extended"
                                    | "prepended"
                                    | "method_added"
                                    | "method_removed"
                                    | "method_undefined"
                                    | "singleton_method_added"
                                    | "singleton_method_removed"
                                    | "singleton_method_undefined"
                            )
                        }) {
                            return ControlFlow::Break(());
                        }
                        if node.kind() == "method"
                            && text(name, source) == Some("initialize")
                            && !direct_class_item(node)
                        {
                            return ControlFlow::Break(());
                        }
                    }
                    let start = if node.kind() == "singleton_method" {
                        // receiver は定義時に評価され、引数の既定値と本体だけが遅延する。
                        node.child_by_field_name("parameters")
                            .or_else(|| node.child_by_field_name("body"))
                            .map_or(node.end_byte(), |body| body.start_byte())
                    } else {
                        node.start_byte()
                    };
                    deferred.push((depth, start));
                }
                "unary"
                    if node
                        .child_by_field_name("operator")
                        .and_then(|operator| text(operator, source))
                        == Some("defined?") =>
                {
                    // 被演算子から肯定は返さず、内部の dispatch 障壁の監査は続ける。
                    unevaluated.push(depth);
                }
                "singleton_class" | "alias" | "undef" => return ControlFlow::Break(()),
                "assignment" | "operator_assignment" => {
                    if let Some(left) = node.child_by_field_name("left") {
                        let bindings = constant_assignments(left, source);
                        let Some(bindings) = bindings else {
                            return ControlFlow::Break(());
                        };
                        for name in bindings {
                            if self
                                .bind(qualify(namespace, &name), Binding::Unknown)
                                .is_none()
                            {
                                return ControlFlow::Break(());
                            }
                        }
                    }
                }
                "for" => {
                    if let Some(pattern) = node.child_by_field_name("pattern") {
                        let Some(bindings) = constant_assignments(pattern, source) else {
                            return ControlFlow::Break(());
                        };
                        for name in bindings {
                            if self
                                .bind(qualify(namespace, &name), Binding::Unknown)
                                .is_none()
                            {
                                return ControlFlow::Break(());
                            }
                        }
                    }
                }
                "identifier" if text(node, source).is_some_and(changes_dispatch) => {
                    // 引数のない private 等は call ではなく identifier として現れる。
                    return ControlFlow::Break(());
                }
                "call" => {
                    let Some(method) = node.child_by_field_name("method") else {
                        return ControlFlow::Break(());
                    };
                    let Some(method_name) = text(method, source) else {
                        return ControlFlow::Break(());
                    };
                    if changes_dispatch(method_name) {
                        return ControlFlow::Break(());
                    }
                    if method_name == "new"
                        && unevaluated.is_empty()
                        && scopes.iter().all(|(_, _, safe)| *safe)
                        && let Some(receiver) = node.child_by_field_name("receiver")
                        && let Some(receiver) = constant_path(receiver, source)
                    {
                        let lexical = namespace.to_string();
                        self.fact_bytes = self
                            .fact_bytes
                            .saturating_add(file.len())
                            .saturating_add(lexical.len())
                            .saturating_add(receiver.parts.iter().map(String::len).sum::<usize>());
                        if self.fact_bytes > MAX_FACT_BYTES {
                            return ControlFlow::Break(());
                        }
                        self.calls.push(ConstructorCall {
                            file: file.to_string(),
                            receiver,
                            lexical,
                            start: node.start_byte(),
                            deferred: deferred
                                .iter()
                                .any(|(_, start)| node.start_byte() >= *start),
                            test,
                        });
                    }
                }
                _ => {}
            }
            ControlFlow::Continue(Walk::Descend)
        });
        (!result.is_break()).then_some(())
    }

    fn resolve(self, candidates: &HashMap<(String, String), usize>) -> RubyConstructorLiveness {
        let mut result = RubyConstructorLiveness::default();
        for call in &self.calls {
            let Some(class) = self.resolve_call(call) else {
                continue;
            };
            let key = (class.file.clone(), class.public_name.clone());
            if candidates.get(&key) != Some(&1) || self.public_declarations.get(&key) != Some(&1) {
                continue;
            }
            let count = result.counts.entry(key).or_default();
            if call.test {
                count.1 = count.1.saturating_add(1);
            } else {
                count.0 = count.0.saturating_add(1);
            }
        }
        result
    }

    fn resolve_call(&self, call: &ConstructorCall) -> Option<&Class> {
        let first = call.receiver.parts.first()?;
        let first_path = if call.receiver.absolute {
            first.clone()
        } else {
            // module/別の定数も束縛として扱い、存在する近い名前で探索を止める。
            let mut namespace = call.lexical.as_str();
            loop {
                let path = qualify(namespace, first);
                if self.bindings.contains_key(&path) {
                    break path;
                }
                if namespace.is_empty() {
                    return None;
                }
                namespace = namespace.rsplit_once("::").map_or("", |(prefix, _)| prefix);
            }
        };
        let mut path = first_path;
        for part in call.receiver.parts.iter().skip(1) {
            match self.bindings.get(&path)? {
                Binding::Module => {}
                Binding::Class(index) if self.classes[*index].supported => {}
                _ => return None,
            }
            path = qualify(&path, part);
        }
        let Binding::Class(index) = self.bindings.get(&path)? else {
            return None;
        };
        // 字句探索で途中の名前を省いた場合も、その名前空間の同一性を確かめる。
        let mut ancestor = path.as_str();
        while let Some((prefix, _)) = ancestor.rsplit_once("::") {
            match self.bindings.get(prefix)? {
                Binding::Module => {}
                Binding::Class(index) if self.classes[*index].supported => {}
                _ => return None,
            }
            ancestor = prefix;
        }
        let class = &self.classes[*index];
        let initializer_end = class.initializer_end?;
        if !class.supported
            || (!call.deferred && call.file == class.file && call.start < initializer_end)
        {
            return None;
        }
        Some(class)
    }
}

fn text<'a>(node: tree_sitter::Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    node.utf8_text(source).ok()
}

fn qualify(namespace: &str, name: &str) -> String {
    if namespace.is_empty() {
        name.to_string()
    } else {
        format!("{namespace}::{name}")
    }
}

fn direct_namespace_item(node: tree_sitter::Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    parent.kind() == "program"
        || (parent.kind() == "body_statement"
            && parent
                .parent()
                .is_some_and(|owner| matches!(owner.kind(), "class" | "module")))
}

fn direct_class_item(node: tree_sitter::Node<'_>) -> bool {
    node.parent().is_some_and(|parent| {
        parent.kind() == "body_statement"
            && parent.parent().is_some_and(|owner| owner.kind() == "class")
    })
}

fn direct_initializer(class: tree_sitter::Node<'_>, source: &[u8]) -> Option<usize> {
    let body = class.child_by_field_name("body")?;
    let mut cursor = body.walk();
    let mut found = None;
    for child in body.named_children(&mut cursor) {
        if matches!(child.kind(), "method" | "singleton_method")
            && child
                .child_by_field_name("name")
                .and_then(|name| text(name, source))
                == Some("initialize")
        {
            if child.kind() != "method" || found.is_some() {
                return None;
            }
            found = Some(child.end_byte());
        }
    }
    found
}

fn constant_path(mut node: tree_sitter::Node<'_>, source: &[u8]) -> Option<ConstantPath> {
    let mut parts = Vec::new();
    let mut absolute = false;
    for _ in 0..MAX_DEPTH {
        match node.kind() {
            "constant" => {
                parts.push(text(node, source)?.to_string());
                break;
            }
            "scope_resolution" => {
                let name = node.child_by_field_name("name")?;
                if name.kind() != "constant" {
                    return None;
                }
                parts.push(text(name, source)?.to_string());
                if let Some(scope) = node.child_by_field_name("scope") {
                    node = scope;
                } else {
                    absolute = true;
                    break;
                }
            }
            _ => return None,
        }
    }
    if parts.is_empty() || parts.iter().any(|part| !part.is_ascii()) {
        return None;
    }
    parts.reverse();
    Some(ConstantPath { parts, absolute })
}

fn constant_assignments(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<Vec<String>> {
    let mut names = Vec::new();
    let result = walk_preorder(node, Children::Named, |node, _| {
        if node.kind() == "scope_resolution" {
            return ControlFlow::Break(());
        }
        if node.kind() == "constant" {
            let Some(name) = text(node, source).filter(|name| name.is_ascii()) else {
                return ControlFlow::Break(());
            };
            names.push(name.to_string());
        }
        ControlFlow::Continue(Walk::Descend)
    });
    (!result.is_break()).then_some(names)
}

fn changes_dispatch(name: &str) -> bool {
    matches!(
        name,
        "include"
            | "attr"
            | "attr_reader"
            | "attr_writer"
            | "attr_accessor"
            | "prepend"
            | "extend"
            | "refine"
            | "using"
            | "define_method"
            | "define_singleton_method"
            | "alias_method"
            | "remove_method"
            | "undef_method"
            | "public"
            | "private"
            | "protected"
            | "public_class_method"
            | "private_class_method"
            | "module_function"
            | "const_set"
            | "remove_const"
            | "autoload"
            | "eval"
            | "class_eval"
            | "module_eval"
            | "instance_eval"
            | "class_exec"
            | "module_exec"
            | "instance_exec"
            | "send"
            | "__send__"
            | "public_send"
            | "method"
            | "singleton_method"
            | "instance_method"
            | "require"
            | "require_relative"
            | "load"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn liveness(sources: &[(&str, &str)], candidate_files: &[&str]) -> RubyConstructorLiveness {
        let candidates = candidate_files
            .iter()
            .map(|file| (*file, "Builder.initialize"))
            .collect::<Vec<_>>();
        named_liveness(sources, &candidates)
    }

    fn named_liveness(
        sources: &[(&str, &str)],
        candidate_names: &[(&str, &str)],
    ) -> RubyConstructorLiveness {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        let mut files = Vec::new();
        for (file, source) in sources {
            let path = canonical.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, source).unwrap();
            files.push(path);
        }
        let candidates = candidate_names
            .iter()
            .map(|(file, name)| {
                (
                    name.to_string(),
                    "function".to_string(),
                    file.to_string(),
                    LangId::Ruby,
                )
            })
            .collect::<Vec<_>>();
        RubyConstructorLiveness::build(&candidates, &canonical, &files, |path| {
            path.strip_prefix(&canonical)
                .unwrap()
                .components()
                .any(|part| part.as_os_str() == "tests")
        })
    }

    #[test]
    fn ruby_constructor_defined_operands_add_no_evidence() {
        for operand in [
            "defined?(Builder.new)",
            "defined? Builder.new",
            "defined?(Builder.new.marker)",
        ] {
            let source = format!("class Builder; def initialize; end; end; {operand}");
            let proof = liveness(&[("builder.rb", &source)], &["builder.rb"]);
            assert_eq!(
                proof.counts_for("builder.rb", "Builder.initialize"),
                (0, 0),
                "{operand}"
            );
        }
        let proof = liveness(
            &[(
                "builder.rb",
                "class Builder; def initialize; end; end; !Builder.new",
            )],
            &["builder.rb"],
        );
        assert_eq!(proof.counts_for("builder.rb", "Builder.initialize"), (1, 0));
    }

    #[test]
    fn ruby_constructor_singleton_receiver_uses_the_enclosing_execution_context() {
        let early = "class Builder; def (Builder.new).marker; end; def initialize; end; end";
        let proof = liveness(&[("builder.rb", early)], &["builder.rb"]);
        assert_eq!(proof.counts_for("builder.rb", "Builder.initialize"), (0, 0));
        for late in [
            "class Builder; def initialize; end; def (Builder.new).marker; end; end",
            "class Builder; def self.make; Builder.new; end; def initialize; end; end",
            "class Builder; def self.make(value = Builder.new); end; def initialize; end; end",
        ] {
            let proof = liveness(&[("builder.rb", late)], &["builder.rb"]);
            assert_eq!(
                proof.counts_for("builder.rb", "Builder.initialize"),
                (1, 0),
                "{late}"
            );
        }
    }

    #[test]
    fn ruby_constructor_attribute_mutation_adds_no_evidence() {
        for attribute in ["attr", "attr_reader", "attr_writer", "attr_accessor"] {
            let source = format!(
                "class Builder; def initialize; end; {attribute}('initial' + 'ize'); end; Builder.new"
            );
            let proof = liveness(&[("builder.rb", &source)], &["builder.rb"]);
            assert_eq!(
                proof.counts_for("builder.rb", "Builder.initialize"),
                (0, 0),
                "{attribute}"
            );
        }
    }

    #[test]
    fn ruby_constructor_predefined_struct_is_a_class_factory() {
        let proof = named_liveness(
            &[(
                "struct.rb",
                "class Struct; def initialize; end; end; Struct.new(:value)",
            )],
            &[("struct.rb", "Struct.initialize")],
        );
        assert_eq!(proof.counts_for("struct.rb", "Struct.initialize"), (0, 0));
        let source = "module Demo; class Struct; def initialize; end; end; end; Demo::Struct.new";
        let proof = named_liveness(
            &[("struct.rb", source)],
            &[("struct.rb", "Struct.initialize")],
        );
        assert_eq!(proof.counts_for("struct.rb", "Struct.initialize"), (1, 0));
    }

    #[test]
    fn ruby_constructor_node_kinds_exist_in_the_grammar() {
        for kind in [
            "program",
            "body_statement",
            "class",
            "module",
            "method",
            "singleton_method",
            "singleton_class",
            "alias",
            "undef",
            "assignment",
            "operator_assignment",
            "for",
            "unary",
            "call",
            "identifier",
            "constant",
            "scope_resolution",
        ] {
            tree_sitter::Query::new(&LangId::Ruby.ts_language(), &format!("({kind})"))
                .unwrap_or_else(|error| panic!("{kind}: {error}"));
        }
    }

    #[test]
    fn ruby_constructor_static_paths_reach_the_initializer() {
        for source in [
            "class Builder; def initialize; end; end; Builder.new",
            "module Demo; class Builder; def initialize; end; end; def self.build; Builder.new; end; end",
            "module Demo; class Builder; def initialize; end; end; end; Demo::Builder.new",
            "module Demo; class Builder; def initialize; end; end; end; ::Demo::Builder.new",
            "class Builder; def initialize; end; end; module Demo; module Builder; end; def self.build; ::Builder.new; end; end",
            "class Builder; VALUE = 1; def initialize; end; end; Builder.new",
        ] {
            let proof = liveness(&[("builder.rb", source)], &["builder.rb"]);
            assert_eq!(
                proof.counts_for("builder.rb", "Builder.initialize"),
                (1, 0),
                "{source}"
            );
        }
    }

    #[test]
    fn ruby_constructor_cross_file_counts_preserve_owner_and_test_origin() {
        let proof = liveness(
            &[
                (
                    "a.rb",
                    "module A; class Builder; def initialize; end; end; end",
                ),
                (
                    "b.rb",
                    "module B; class Builder; def initialize; end; end; end",
                ),
                ("use.rb", "A::Builder.new"),
                ("tests/use.rb", "B::Builder.new"),
            ],
            &["a.rb", "b.rb"],
        );
        assert_eq!(proof.counts_for("a.rb", "Builder.initialize"), (1, 0));
        assert_eq!(proof.counts_for("b.rb", "Builder.initialize"), (0, 1));
    }

    #[test]
    fn ruby_constructor_nearer_constant_bindings_prevent_outer_fallback() {
        for binding in [
            "module Builder; end",
            "class Builder; end",
            "Builder = String",
            "Builder, Other = [String, Object]",
            "Builder ||= String",
            "for Builder in [String]; end",
        ] {
            let source = format!(
                "class Builder; def initialize; end; end; module Demo; {binding}; def self.build; Builder.new; end; end"
            );
            let proof = liveness(&[("builder.rb", &source)], &["builder.rb"]);
            assert_eq!(
                proof.counts_for("builder.rb", "Builder.initialize"),
                (0, 0),
                "{binding}"
            );
        }
    }

    #[test]
    fn ruby_constructor_unsupported_dispatch_adds_no_evidence() {
        for source in [
            "class Builder; def initialize; end; end; Builder",
            "class Builder; def initialize; end; end; Other.new",
            "class Builder; def initialize; end; def self.new; allocate; end; end; Builder.new",
            "class Builder; def initialize; end; end; class << Builder; def new; allocate; end; end; Builder.new",
            "class Parent; end; class Builder < Parent; def initialize; end; end; Builder.new",
            "class Builder; def initialize; end; end; class Builder; end; Builder.new",
            "class Builder; def initialize; end; end; Builder = String; Builder.new",
            "class Builder; def initialize; end; end; module Bypass; refine Class do; def new; nil; end; end; end; using Bypass; Builder.new",
            "class Builder; def initialize; end; alias make new; end; Builder.new",
            "class Builder; def initialize; end; undef old; end; Builder.new",
            "class Builder; def initialize; end; private; end; Builder.new",
            "class Builder; def initialize; end; end; SomeOwner.define_method(:new) {} ; Builder.new",
            "class Builder; def initialize; end; end; Other.const_set(:X, String); Builder.new",
            "class Builder; def initialize; end; end; require 'external'; Builder.new",
            "class Builder; if true; def initialize; end; end; end; Builder.new",
            "class Builder; def self.initialize; end; end; Builder.new",
            "class Builder; def initialize; end; def initialize; end; end; Builder.new",
            "module Demo; end; class Demo::Builder; def initialize; end; end; Demo::Builder.new",
            "class Builder; def initialize; end; end; module Demo; end; module Demo::Nested; Builder.new; end",
            "class Builder; def initialize; end; end; class Context < External; def self.build; Builder.new; end; end",
        ] {
            let proof = liveness(&[("builder.rb", source)], &["builder.rb"]);
            assert_eq!(
                proof.counts_for("builder.rb", "Builder.initialize"),
                (0, 0),
                "{source}"
            );
        }
    }

    #[test]
    fn ruby_constructor_direct_calls_before_the_definition_are_unproved() {
        for source in [
            "class Builder; Builder.new; def initialize; end; end",
            "class Builder; 1.times { Builder.new }; def initialize; end; end",
            "Builder.new; class Builder; def initialize; end; end",
        ] {
            let proof = liveness(&[("builder.rb", source)], &["builder.rb"]);
            assert_eq!(
                proof.counts_for("builder.rb", "Builder.initialize"),
                (0, 0),
                "{source}"
            );
        }
    }

    #[test]
    fn ruby_constructor_public_identity_collisions_are_unproved() {
        let source = "module A; class Builder; def initialize; end; end; end; module B; class Builder; def initialize; end; end; end; A::Builder.new";
        let proof = liveness(&[("builder.rb", source)], &["builder.rb"]);
        assert_eq!(proof.counts_for("builder.rb", "Builder.initialize"), (0, 0));
        let proof = liveness(
            &[
                (
                    "builder.rb",
                    "class Builder; def initialize; end; end; Builder.new",
                ),
                ("generated.rb", "# @generated\nclass Builder; end"),
            ],
            &["builder.rb"],
        );
        assert_eq!(proof.counts_for("builder.rb", "Builder.initialize"), (0, 0));
    }

    #[test]
    fn ruby_constructor_late_read_parse_and_size_failures_discard_all_counts() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        let valid = canonical.join("builder.rb");
        let invalid = canonical.join("later.rb");
        std::fs::write(
            &valid,
            "class Builder; def initialize; end; end; Builder.new",
        )
        .unwrap();
        let candidates = vec![(
            "Builder.initialize".into(),
            "function".into(),
            "builder.rb".into(),
            LangId::Ruby,
        )];
        let build = || {
            RubyConstructorLiveness::build(
                &candidates,
                &canonical,
                &[valid.clone(), invalid.clone()],
                |_| false,
            )
        };
        assert_eq!(
            build().counts_for("builder.rb", "Builder.initialize"),
            (0, 0)
        );
        std::fs::write(&invalid, "class").unwrap();
        assert_eq!(
            build().counts_for("builder.rb", "Builder.initialize"),
            (0, 0)
        );
        std::fs::write(&invalid, vec![b' '; MAX_FILE_BYTES + 1]).unwrap();
        assert_eq!(
            build().counts_for("builder.rb", "Builder.initialize"),
            (0, 0)
        );
    }
}
