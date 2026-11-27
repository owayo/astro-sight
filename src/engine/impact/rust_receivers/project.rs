//! 相対・crate 起点の import を、通常の Cargo モジュールの実宣言と照合する。

use std::cell::{OnceCell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use super::super::rust_owner_alias::{Names, items_with_attributes};
use super::super::rust_path_owner::{PrivateModule, RustPathOwnerProjectProof};
use crate::engine::bounded_read::read_input_file;
use crate::engine::parser;
use crate::engine::tree_walk::{Children, Walk, walk_preorder};
use crate::language::LangId;
use tree_sitter::Node;

type ModuleCache = HashMap<(PathBuf, bool), Option<Rc<Module>>>;

pub(in crate::engine::impact) struct Project<'a> {
    pub(super) layout: &'a RustPathOwnerProjectProof,
    old_macro_names: OnceCell<Option<HashSet<String>>>,
    previous_layout: OnceCell<bool>,
    modules: RefCell<ModuleCache>,
}

impl<'a> Project<'a> {
    pub(in crate::engine::impact) fn new(layout: &'a RustPathOwnerProjectProof) -> Self {
        Self {
            layout,
            old_macro_names: OnceCell::new(),
            previous_layout: OnceCell::new(),
            modules: Default::default(),
        }
    }

    pub(super) fn previous_layout_known(&self) -> bool {
        *self.previous_layout.get_or_init(|| {
            self.old_macro_names()
                .is_some_and(|names| !names.contains("include_str"))
                && self.layout.previous_receiver_layout_known()
        })
    }

    /// 標準名を差し替える旧側の import があれば保留し、旧側のマクロの名前も集める。
    pub(super) fn old_macro_names(&self) -> Option<&HashSet<String>> {
        self.old_macro_names
            .get_or_init(|| {
                let mut names = HashSet::new();
                for (file, source) in &self.layout.reexport_old_sources {
                    if file == Path::new("/dev/null") {
                        continue;
                    }
                    let source = source.as_deref()?;
                    let macros = memchr::memmem::find(source, b"macro_rules").is_some();
                    if !macros
                        && memchr::memmem::find(source, b"extern").is_none()
                        && memchr::memmem::find(source, b"macro_use").is_none()
                    {
                        continue;
                    }
                    let tree = parser::parse_source(source, LangId::Rust).ok()?;
                    let root = tree.root_node();
                    if root.has_error()
                        || super::super::rust_std_origin::has_foreign_std_alias(root, source)
                    {
                        return None;
                    }
                    if macros {
                        let _: std::ops::ControlFlow<()> =
                            walk_preorder(root, Children::Named, |node, _| {
                                if node.kind() == "macro_definition"
                                    && let Some(name) = node
                                        .child_by_field_name("name")
                                        .and_then(|n| super::text(n, source))
                                {
                                    names.insert(name.to_owned());
                                }
                                std::ops::ControlFlow::Continue(Walk::Descend)
                            });
                    }
                }
                Some(names)
            })
            .as_ref()
    }

    pub(super) fn stock_std(&self, file: &str, old: bool) -> bool {
        self.layout
            .root
            .join(file)
            .to_str()
            .is_some_and(|file| self.layout.std_project.allows(file))
            && (!old || self.old_macro_names().is_some())
    }

    pub(super) fn module(
        &self,
        owner: &PrivateModule,
        path: &[String],
        old: bool,
    ) -> Option<Rc<Module>> {
        let file = self.layout.declared_module_file(&owner.src, path)?;
        if old {
            for i in 0..path.len() {
                let parent = if i == 0 {
                    owner.src.join("lib.rs")
                } else {
                    self.layout.declared_module_file(&owner.src, &path[..i])?
                };
                if self.layout.reexport_old_sources.contains_key(&parent)
                    && !self
                        .load(&parent, &path[..i], true)?
                        .modules
                        .contains(&path[i])
                {
                    return None;
                }
            }
        }
        self.load(&file, path, old)
    }

    fn load(&self, file: &Path, module_path: &[String], old: bool) -> Option<Rc<Module>> {
        let old_text = old
            .then(|| self.layout.reexport_old_sources.get(file))
            .flatten();
        let key = (file.to_owned(), old_text.is_some());
        self.modules
            .borrow_mut()
            .entry(key)
            .or_insert_with(|| {
                let current;
                let bytes = if let Some(old) = old_text {
                    old.as_deref()?
                } else {
                    current = read_input_file(&self.layout.root.join(file))?;
                    &current
                };
                let tree = parser::parse_source(bytes, LangId::Rust).ok()?;
                Module::collect(tree.root_node(), bytes, module_path).map(Rc::new)
            })
            .clone()
    }
}

#[derive(Default)]
pub(super) struct Module {
    modules: HashSet<String>,
    pub(super) nominals: HashSet<String>,
    pub(super) nontraits: HashSet<String>,
    pub(super) factories: HashSet<(String, String)>,
    pub(super) methods: HashSet<(String, String)>,
}

impl Module {
    fn collect(root: Node<'_>, source: &[u8], module_path: &[String]) -> Option<Self> {
        if root.has_error() {
            return None;
        }
        let mut module = Self {
            nontraits: super::concrete_types(root, source),
            ..Self::default()
        };
        let traits: HashSet<_> = root
            .named_children(&mut root.walk())
            .filter(|item| item.kind() == "trait_item")
            .filter_map(|item| super::text(item.child_by_field_name("name")?, source))
            .collect();
        let names = OnceCell::new();
        let mut cursor = root.walk();
        for (item, attributed) in items_with_attributes(root.named_children(&mut cursor)) {
            if attributed {
                continue;
            }
            if item.kind() == "impl_item" {
                module.impl_methods(item, source);
                continue;
            }
            let Some(name) = item
                .child_by_field_name("name")
                .and_then(|n| super::text(n, source))
            else {
                continue;
            };
            match item.kind() {
                "struct_item" | "enum_item" | "union_item" => {
                    module.nominals.insert(name.to_owned());
                }
                "mod_item" => {
                    if item.child_by_field_name("body").is_none() {
                        module.modules.insert(name.to_owned());
                    }
                }
                "function_item" => {
                    if names
                        .get_or_init(|| Names::collect(root, source, module_path))
                        .as_ref()
                        .is_some_and(|n| n.globs.is_empty() && !n.imports.contains_key(name))
                        && !traits.contains(name)
                    {
                        module.nontraits.insert(name.to_owned());
                    }
                }
                _ => continue,
            };
            if item.kind() != "function_item" {
                module.nontraits.insert(name.to_owned());
            }
        }
        Some(module)
    }

    fn impl_methods(&mut self, item: Node<'_>, source: &[u8]) {
        if item.child_by_field_name("trait").is_some() {
            return;
        }
        let Some(mut ty) = item.child_by_field_name("type") else {
            return;
        };
        if ty.kind() == "generic_type" {
            let Some(args) = ty.child_by_field_name("type_arguments") else {
                return;
            };
            let Some(params) = item.child_by_field_name("type_parameters") else {
                return;
            };
            let Some(param) = params.named_child(0) else {
                return;
            };
            if args.named_child_count() != 1
                || params.named_child_count() != 1
                || param.kind() != "lifetime_parameter"
                || !args.named_child(0).is_some_and(|arg| {
                    arg.kind() == "lifetime"
                        && arg.utf8_text(source).ok() == param.utf8_text(source).ok()
                })
            {
                return;
            }
            let Some(inner) = ty.child_by_field_name("type") else {
                return;
            };
            ty = inner;
        } else if item.child_by_field_name("type_parameters").is_some() {
            return;
        }
        if ty.kind() != "type_identifier"
            || item
                .named_children(&mut item.walk())
                .any(|n| n.kind() == "where_clause")
        {
            return;
        }
        let Some(owner) = super::text(ty, source) else {
            return;
        };
        let Some(body) = item.child_by_field_name("body") else {
            return;
        };
        let mut cursor = body.walk();
        for (method, attributed) in items_with_attributes(body.named_children(&mut cursor)) {
            if attributed
                || method.kind() != "function_item"
                || method
                    .named_children(&mut method.walk())
                    .any(|n| n.kind() == "where_clause")
            {
                continue;
            }
            let Some(name) = method
                .child_by_field_name("name")
                .and_then(|n| super::text(n, source))
            else {
                continue;
            };
            let Some(params) = method.child_by_field_name("parameters") else {
                continue;
            };
            let receiver = params
                .named_children(&mut params.walk())
                .find(|n| !n.is_extra());
            if receiver
                .is_some_and(|n| n.kind() == "self_parameter" && n.utf8_text(source) == Ok("&self"))
            {
                self.methods.insert((owner.to_owned(), name.to_owned()));
            } else if !receiver.is_some_and(|n| n.kind() == "self_parameter")
                && method
                    .child_by_field_name("return_type")
                    .is_some_and(|ty| ty.utf8_text(source) == Ok("Self"))
            {
                self.factories.insert((owner.to_owned(), name.to_owned()));
            }
        }
    }
}
