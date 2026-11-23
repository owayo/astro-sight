//! 通常のモジュール宣言と inherent impl の証拠を解析要求内で共有する。

use super::*;
use std::sync::{Arc, Mutex, OnceLock};

type Cached<T> = Arc<OnceLock<Option<Arc<T>>>>;
struct Files<T>(Mutex<HashMap<PathBuf, Cached<T>>>);

impl<T> Default for Files<T> {
    fn default() -> Self {
        Self(Mutex::default())
    }
}

impl<T> Files<T> {
    fn get(&self, path: &Path, load: impl FnOnce() -> Option<T>) -> Option<Arc<T>> {
        let cell = self
            .0
            .lock()
            .expect("Rust evidence cache poisoned")
            .entry(path.to_owned())
            .or_default()
            .clone();
        cell.get_or_init(|| load().map(Arc::new)).clone()
    }
}

#[derive(Default)]
pub(in super::super) struct PackageCache {
    method_libraries: HashSet<PathBuf>,
    manifests: Files<toml::Value>,
    files: Files<ModuleItems>,
    packages: Files<Package>,
}

pub(super) struct Package {
    pub(super) manifest: Arc<toml::Value>,
    pub(super) workspace: Arc<toml::Value>,
    pub(super) library: Arc<ModuleItems>,
    pub(super) directory: PathBuf,
    pub(super) upstream: HashSet<String>,
}

impl PackageCache {
    pub(in super::super) fn track_method_source(&mut self, root: &Path, file: &Path) {
        if let Some(package) = file
            .ancestors()
            .skip(1)
            .find(|p| root.join(p).join("Cargo.toml").is_file())
        {
            self.method_libraries.insert(package.join("src/lib.rs"));
        }
    }

    pub(in super::super) fn manifest(
        &self,
        root: &Path,
        directory: &Path,
    ) -> Option<Arc<toml::Value>> {
        self.manifests.get(directory, || {
            toml::from_str(&read_input_text_file(
                &root.join(directory).join("Cargo.toml"),
            )?)
            .ok()
        })
    }

    pub(in super::super) fn module(&self, root: &Path, file: &Path) -> Option<Arc<ModuleItems>> {
        self.files.get(file, || {
            let data = read_input_file(&root.join(file))?;
            let tree = parser::parse_source(&data, LangId::Rust).ok()?;
            let node = tree.root_node();
            (!node.has_error()).then(|| {
                ModuleItems::collect(
                    node,
                    &data,
                    file.ends_with("src/lib.rs") && !self.method_libraries.is_empty(),
                    self.method_libraries.contains(file),
                )
            })
        })
    }

    pub(super) fn package(
        &self,
        project: &RustPathOwnerProjectProof,
        src: &Path,
    ) -> Option<Arc<Package>> {
        self.packages.get(src, || {
            let lib = src.join("lib.rs");
            let directory = src.parent()?;
            if project.reexport_old_sources.contains_key(&lib)
                || !project
                    .cargo_configs
                    .keeps_dependency_sources(&project.root.join(directory))
            {
                return None;
            }
            let manifest = self.manifest(&project.root, directory)?;
            let workspace = self.manifest(&project.root, Path::new(""))?;
            if [&manifest, &workspace]
                .iter()
                .any(|m| m.get("patch").is_some() || m.get("replace").is_some())
            {
                return None;
            }
            let package = manifest.get("package")?;
            let edition = package.get("edition")?;
            let edition = if edition.get("workspace").and_then(toml::Value::as_bool) == Some(true) {
                workspace.get("workspace")?.get("package")?.get("edition")?
            } else {
                edition
            };
            if !matches!(edition.as_str()?, "2018" | "2021" | "2024") {
                return None;
            }
            let name = manifest
                .get("lib")
                .and_then(|lib| lib.get("name"))
                .or_else(|| package.get("name"))?
                .as_str()?
                .replace('-', "_");
            let mut upstream: HashSet<_> = ["std", "core", "alloc"].map(str::to_owned).into();
            if let Some(deps) = manifest.get("dependencies").and_then(toml::Value::as_table) {
                upstream.extend(deps.keys().map(|s| s.replace('-', "_")));
            }
            upstream.remove(&name);
            Some(Package {
                manifest,
                workspace,
                directory: directory.to_owned(),
                upstream,
                library: self.module(&project.root, &lib)?,
            })
        })
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) enum ReceiverBound {
    Concrete,
    Generic,
    Trait(String),
}

type Methods = HashMap<String, HashMap<String, HashSet<ReceiverBound>>>;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Declaration {
    Nominal,
    Module,
    Trait,
    Alias,
}

#[derive(Default)]
pub(in super::super) struct ModuleItems {
    pub(in super::super) external_modules: HashMap<String, usize>,
    declarations: HashMap<String, Option<Declaration>>,
    pub(super) names: Option<Names>,
    pub(super) methods: Methods,
    pub(super) implementations: HashMap<String, HashSet<String>>,
}

impl ModuleItems {
    fn collect(root: Node<'_>, data: &[u8], library: bool, methods: bool) -> Self {
        let mut result = Self {
            names: methods.then(|| Names::collect(root, data, &[])).flatten(),
            ..Default::default()
        };
        let mut cursor = root.walk();
        for (item, attributed) in items_with_attributes(root.named_children(&mut cursor)) {
            let kind = item.kind();
            if !library && kind != "mod_item" {
                continue;
            }
            if kind == "impl_item" && !attributed && !has_child(item, "where_clause") {
                if let Some(tr) = field(item, "trait", data) {
                    if !has_child(item, "!")
                        && item.child_by_field_name("type_parameters").is_none()
                        && let Some(name) = field(item, "type", data)
                    {
                        result
                            .implementations
                            .entry(name.to_owned())
                            .or_default()
                            .insert(tr.to_owned());
                    }
                } else if methods {
                    result.collect_methods(item, data);
                }
            }
            if !matches!(
                kind,
                "mod_item"
                    | "struct_item"
                    | "enum_item"
                    | "union_item"
                    | "trait_item"
                    | "type_item"
            ) {
                continue;
            }
            let Some(name) = item
                .child_by_field_name("name")
                .and_then(|n| plain_name(n, data))
            else {
                continue;
            };
            if kind == "mod_item" && item.child_by_field_name("body").is_none() {
                *result.external_modules.entry(name.to_owned()).or_default() += 1;
            }
            result
                .declarations
                .entry(name.to_owned())
                .and_modify(|v| *v = None)
                .or_insert((!attributed).then_some(match kind {
                    "mod_item" => Declaration::Module,
                    "trait_item" => Declaration::Trait,
                    "type_item" => Declaration::Alias,
                    _ => Declaration::Nominal,
                }));
        }
        result
    }

    pub(super) fn declares(&self, name: &str, kind: Declaration) -> bool {
        self.declarations.get(name) == Some(&Some(kind))
    }

    fn collect_methods(&mut self, item: Node<'_>, data: &[u8]) -> Option<()> {
        let (name, formal) = simple_type(item.child_by_field_name("type")?, data)?;
        let bound = if let Some(formal) = formal {
            let param = only_child(item.child_by_field_name("type_parameters")?)?;
            if param.kind() != "type_parameter" || field(param, "name", data)? != formal {
                return None;
            }
            if let Some(bounds) = param.child_by_field_name("bounds") {
                let bound = only_child(bounds)?;
                if bound.kind() != "type_identifier" {
                    return None;
                }
                ReceiverBound::Trait(plain_name(bound, data)?.to_owned())
            } else {
                ReceiverBound::Generic
            }
        } else {
            ReceiverBound::Concrete
        };
        let body = item.child_by_field_name("body")?;
        let mut cursor = body.walk();
        for (m, attributed) in items_with_attributes(body.named_children(&mut cursor)) {
            if !attributed
                && m.kind() == "function_item"
                && !has_child(m, "where_clause")
                && m.child_by_field_name("type_parameters").is_none()
                && m.child_by_field_name("parameters")
                    .and_then(|p| p.named_child(0))
                    .is_some_and(|n| n.kind() == "self_parameter")
                && let Some(method) = field(m, "name", data)
            {
                self.methods
                    .entry(name.to_owned())
                    .or_default()
                    .entry(method.to_owned())
                    .or_default()
                    .insert(bound.clone());
            }
        }
        Some(())
    }
}

pub(super) fn items_with_attributes<'a>(
    items: impl Iterator<Item = Node<'a>>,
) -> impl Iterator<Item = (Node<'a>, bool)> {
    let mut attributed = false;
    items.filter_map(move |item| match item.kind() {
        "attribute_item" => {
            attributed = true;
            None
        }
        "line_comment" | "block_comment" => None,
        _ => Some((item, std::mem::take(&mut attributed))),
    })
}
