//! Conservative owner proof for Rust type paths used only by impact.
//!
//! A proof requires an ordinary Cargo source layout, an unambiguous chain of
//! `mod` declarations, and a local declaration in the resolved module. Any
//! source transformation or uncertain binding leaves the reference unchanged.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use tree_sitter::Node;

use crate::engine::{
    bounded_read::{read_input_file, read_input_text_file},
    parser,
    rust_macro_name::{
        has_token_tree_ancestor, is_field_name_in_token_tree, last_macro_identifier,
    },
    rust_targets::RustTargets,
};
use crate::language::LangId;
use crate::models::impact::DiffFile;

mod enum_owner;
mod format_owner;
mod function_pointer;
mod macro_copy;
mod macro_crate;
mod nominal;
mod registry_macros;
mod tracing_fields;
mod visibility_names;
mod workspace_members;

pub(super) use macro_copy::MacroCopy;
pub(super) use macro_crate::RuleName;

struct FileEvidence {
    types: HashSet<String>,
    free_functions: HashSet<String>,
    modules: HashSet<String>,
}

impl FileEvidence {
    fn collect(root: Node<'_>, source: &[u8]) -> Option<Self> {
        if root.kind() != "source_file" || root.has_error() {
            return None;
        }
        let mut imports = Vec::new();
        let mut pending = vec![root];
        while let Some(node) = pending.pop() {
            if matches!(
                node.kind(),
                "attribute_item"
                    | "inner_attribute_item"
                    | "macro_invocation"
                    | "macro_definition"
                    | "extern_crate_declaration"
                    | "type_parameters"
            ) {
                return None;
            }
            if node.kind() == "use_declaration" {
                imports.push(node.utf8_text(source).ok()?.to_owned());
            }
            if matches!(
                node.kind(),
                "struct_item" | "enum_item" | "union_item" | "type_item" | "trait_item"
            ) && node.parent().is_none_or(|parent| parent.id() != root.id())
            {
                return None;
            }
            if node.kind() == "mod_item" && node.child_by_field_name("body").is_some() {
                return None;
            }
            let mut cursor = node.walk();
            pending.extend(node.named_children(&mut cursor));
        }
        let mut types = HashSet::new();
        let mut free_functions = HashSet::new();
        let mut modules = HashSet::new();
        let import_may_bind = |name: &str| {
            imports
                .iter()
                .any(|import| !import.is_ascii() || import.contains('*') || import.contains(name))
        };
        let mut cursor = root.walk();
        for child in root.named_children(&mut cursor) {
            if matches!(
                child.kind(),
                "struct_item" | "enum_item" | "union_item" | "trait_item"
            ) {
                let name = child.child_by_field_name("name")?.utf8_text(source).ok()?;
                if import_may_bind(name) || !types.insert(name.to_owned()) {
                    return None;
                }
            } else if child.kind() == "function_item" {
                let name = child.child_by_field_name("name")?.utf8_text(source).ok()?;
                if import_may_bind(name) || !free_functions.insert(name.to_owned()) {
                    return None;
                }
            } else if child.kind() == "mod_item" {
                let name = child.child_by_field_name("name")?.utf8_text(source).ok()?;
                if import_may_bind(name) || !modules.insert(name.to_owned()) {
                    return None;
                }
            }
        }
        Some(Self {
            types,
            free_functions,
            modules,
        })
    }
}

#[derive(Clone)]
pub(super) enum QualifiedCallOwner {
    Type,
    Function(PathBuf),
}

pub(super) struct RustPathOwnerIndex {
    current: Option<Arc<FileEvidence>>,
    root_associated_paths: HashSet<usize>,
    unshadowed_qualified_paths: HashSet<usize>,
    associated_types: RefCell<HashMap<String, bool>>,
    external_associated_types: RefCell<HashMap<String, bool>>,
    external_associated_paths: OnceLock<HashSet<usize>>,
    registry_qualifiers: RefCell<HashMap<String, bool>>,
    source_hash: blake3::Hash,
    direct_imports: HashMap<String, Vec<String>>,
    qualified_call_owners: RefCell<HashMap<Vec<String>, Option<QualifiedCallOwner>>>,
    field_types: std::cell::OnceCell<HashSet<String>>,
    macro_paths: std::cell::OnceCell<HashMap<usize, RuleName>>,
    independent_pointers: std::cell::OnceCell<HashSet<usize>>,
    json_calls: RefCell<HashMap<usize, bool>>,
    tracing_fields: std::cell::OnceCell<Option<HashSet<usize>>>,
    visibility_imports: visibility_names::Imports,
    local_type_names: HashSet<String>,
    local_value_names: HashSet<String>,
    closed_local_rule: OnceLock<Option<usize>>,
    module_context: OnceLock<Option<(PathBuf, PathBuf, Vec<String>)>>,
    resolved_paths: RefCell<HashMap<Vec<String>, Option<PathBuf>>>,
    enum_files: RefCell<HashMap<PathBuf, Option<enum_owner::EnumFile>>>,
    enum_owners: RefCell<HashMap<(String, String), Option<PathBuf>>>,
    nominal_sites: std::cell::OnceCell<Option<nominal::NominalSites>>,
    local_inherent_sites: std::cell::OnceCell<HashSet<usize>>,
    format_environment: OnceLock<Option<format_owner::Environment>>,
    /// スコープが名前を再束縛し得るかの判定の結果。同じスコープの中の参照ごとに部分木を
    /// 走査し直さない (大きな関数の本体で参照の数に比例して遅くならないように)。
    rebinds: RefCell<HashMap<(usize, Rebind, String), bool>>,
    /// `source_hash` と一致を確かめたソースの場所と長さ。参照の走査の間 (ソースを借りたまま
    /// 変えない) だけ使い、参照ごとにソース全体のハッシュを計算し直さない。走査が終わってソースを
    /// 手放したら [`Self::forget_verified_source`] で忘れる。
    verified_source: std::cell::Cell<Option<(usize, usize)>>,
}

/// 再束縛の判定の種類 (`rebinds` の鍵)。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Rebind {
    /// [`scope_can_rebind_name`]
    Type,
    /// [`scope_can_rebind_import_name_beside`] と、標準と証明したマクロの呼び出し
    Value(Option<usize>),
    WidenedFunction,
}

/// Module-relative paths need a unique compilation context for each source
/// file. A path attribute or macro elsewhere in the tree can include that
/// source under a different parent module or crate root.
pub(super) type RustSourceModule = (PathBuf, PathBuf, Vec<String>);
pub(super) type RustModuleKey = (PathBuf, Vec<String>);

#[derive(Clone)]
pub(super) struct VisibilityModule {
    pub(super) crate_root: PathBuf,
    pub(super) modules: Vec<String>,
}

/// 直接 import の証明で、import の先の宣言に求める名前空間。参照の構文から決める。
/// import は宣言のある名前空間だけを束縛し、ほかの名前空間の同名は外側のスコープへ解決される。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum ImportNamespace {
    /// import の宣言そのもの。どの名前空間の宣言でもよい。
    Any,
    /// 式の識別子や書式の暗黙キャプチャ。
    Value,
    /// 型の位置の識別子。
    Type,
}

pub(super) type OwnerSource = OnceLock<Option<Arc<PrivateModule>>>;

pub(super) struct PrivateModule {
    pub(super) src: PathBuf,
    pub(super) path: Vec<String>,
    pub(super) library_only: bool,
}

pub(super) struct RustPathOwnerProjectProof {
    pub(super) root: PathBuf,
    pub(super) std_project: super::rust_std_origin::RustStdProjectProof,
    /// staged のときも、Cargo の外部設定は元の checkout の祖先にある。
    pub(super) layout_checkout: PathBuf,
    pub(super) staged_binding_diff: Option<Box<str>>,
    staged_private_call_wiring: OnceLock<bool>,
    callable_cargo_known: bool,
    registry_macros: Mutex<HashMap<String, bool>>,
    lockfile_unchanged: bool,
    callable_layout_stable: OnceLock<bool>,
    macros_may_export_tokens: OnceLock<bool>,
    macro_glob_names: Mutex<HashMap<PathBuf, Option<Arc<HashSet<String>>>>>,
    ordinary_files: Mutex<HashMap<RustModuleKey, Option<PathBuf>>>,
    macro_functions: macro_crate::FunctionFiles,
    foreign_pointers: OnceLock<Option<(PathBuf, HashSet<String>)>>,
    staged_bindings: OnceLock<Option<Box<(crate::commands::git_snapshot::IndexSnapshot, Self)>>>,
    layout_cargo_settings: OnceLock<bool>,
    pub(super) canonical_root: Option<PathBuf>,
    ordinary_sources: OnceLock<bool>,
    private_scope_sources: OnceLock<bool>,
    reexport_sources: OnceLock<bool>,
    safe_private_import_environment: OnceLock<bool>,
    /// module の配置を変え得る構文が無いときの、関数の本体の外の `use` が束縛する名前。
    static_module_bindings: OnceLock<Option<HashSet<String>>>,
    /// 公開し得る属性の付いた `macro_rules!` の名前。走査できなければ `None`。
    macro_definitions: OnceLock<Option<HashSet<String>>>,
    safe_macro_value_environment: OnceLock<bool>,
    no_source_inclusion: OnceLock<bool>,
    /// `include!` を 1 つも許さない配置 ([`enum_owner::static_layout`])。
    static_source_layout: OnceLock<Option<Vec<PathBuf>>>,
    /// 文字列リテラルの `include!` の取り込み先と元を保留して許す配置と、それを受け入れたか、
    /// 木の中に `macro_rules!` の定義があったか。片方の結果からもう片方を導けるときは、走査を
    /// やり直さない ([`Self::static_layout`])。
    private_layout: OnceLock<Option<(Vec<PathBuf>, bool, bool)>>,
    private_modules: Mutex<HashMap<PathBuf, Option<Arc<PrivateModule>>>>,
    relocation: Mutex<HashMap<PathBuf, bool>>,
    changed_sources_unrelocated: OnceLock<bool>,
    /// [`Self::dependency_closure`] の結果 (`from` の package のディレクトリごと)。
    dependency_closures: Mutex<HashMap<PathBuf, Option<Arc<DependencyClosure>>>>,
    /// 木のどこかに `macro_rules! define_class` があるか ([`Self::objc2_define_class`])。
    in_tree_define_class: OnceLock<bool>,
    /// [`RustPathOwnerProjectProof::scan_dependency_sources`] の結果。
    dependency_sources_fixed: OnceLock<bool>,
    pub(super) alias_cache: super::rust_owner_alias::PackageCache,
    owner_changes: OnceLock<Vec<PathBuf>>,
    owner_derive_safe: OnceLock<bool>,
    no_glob_imports: OnceLock<bool>,
    widening_glob_imports: OnceLock<Option<HashSet<String>>>,
    parent_glob_widening_sources: OnceLock<bool>,
    module_wiring_unchanged: bool,
    private_scope_wiring_unchanged: bool,
    reexport_cargo_enabled: bool,
    pub(super) reexport_old_sources: HashMap<PathBuf, Option<Vec<u8>>>,
    pub(super) reexport_source_modules: Mutex<HashMap<PathBuf, Option<RustSourceModule>>>,
    pub(super) reexport_module_files: Mutex<HashMap<RustModuleKey, Option<PathBuf>>>,
    pub(super) reexport_stable_chains: Mutex<HashMap<RustModuleKey, bool>>,
    files: Mutex<HashMap<PathBuf, Option<Arc<FileEvidence>>>>,
    pub(super) reexport_files:
        Mutex<HashMap<PathBuf, Option<Arc<super::rust_reexport_owner::BindingVersions>>>>,
    leaf_sources: Mutex<HashMap<String, bool>>,
    visibility_modules: Mutex<HashMap<PathBuf, Option<VisibilityModule>>>,
    pub(super) cargo_configs: super::rust_cargo_config::CargoConfigProof,
}

impl RustPathOwnerProjectProof {
    pub(super) fn new(
        root: &Path,
        module_wiring_unchanged: bool,
        private_scope_wiring_unchanged: bool,
        reexport_cargo_enabled: bool,
        diff_files: &[DiffFile],
        diff_input: &str,
    ) -> Self {
        let mut reexport_old_sources = HashMap::new();
        for file in diff_files {
            let path = PathBuf::from(&file.new_path);
            if path.extension().is_some_and(|ext| ext == "rs") {
                let old = read_input_file(&root.join(&path)).and_then(|new_source| {
                    crate::engine::diff::reconstruct_old_source(
                        diff_input,
                        &file.new_path,
                        &new_source,
                    )
                    .map(|source| source.source)
                });
                reexport_old_sources.insert(path, old);
            }
            if file.old_path != file.new_path {
                reexport_old_sources.insert(PathBuf::from(&file.old_path), None);
            }
        }
        Self {
            root: root.to_owned(),
            std_project: super::rust_std_origin::RustStdProjectProof::new(root),
            layout_checkout: root.to_owned(),
            staged_binding_diff: None,
            staged_private_call_wiring: OnceLock::new(),
            callable_cargo_known: reexport_cargo_enabled,
            registry_macros: Mutex::default(),
            lockfile_unchanged: !diff_files.iter().any(|file| {
                [&file.old_path, &file.new_path].iter().any(|p| {
                    Path::new(p)
                        .file_name()
                        .is_some_and(|n| n.eq_ignore_ascii_case("Cargo.lock"))
                })
            }),
            callable_layout_stable: OnceLock::new(),
            macros_may_export_tokens: OnceLock::new(),
            macro_glob_names: Mutex::default(),
            ordinary_files: Mutex::default(),
            foreign_pointers: Default::default(),
            macro_functions: Default::default(),
            staged_bindings: OnceLock::new(),
            layout_cargo_settings: OnceLock::new(),
            canonical_root: root.canonicalize().ok(),
            ordinary_sources: OnceLock::new(),
            private_scope_sources: OnceLock::new(),
            reexport_sources: OnceLock::new(),
            safe_private_import_environment: OnceLock::new(),
            static_module_bindings: OnceLock::new(),
            macro_definitions: OnceLock::new(),
            safe_macro_value_environment: OnceLock::new(),
            no_source_inclusion: OnceLock::new(),
            static_source_layout: OnceLock::new(),
            private_layout: OnceLock::new(),
            private_modules: Mutex::new(HashMap::new()),
            relocation: Mutex::new(HashMap::new()),
            changed_sources_unrelocated: OnceLock::new(),
            dependency_closures: Mutex::new(HashMap::new()),
            in_tree_define_class: OnceLock::new(),
            dependency_sources_fixed: OnceLock::new(),
            alias_cache: Default::default(),
            owner_changes: OnceLock::new(),
            owner_derive_safe: OnceLock::new(),
            no_glob_imports: OnceLock::new(),
            widening_glob_imports: OnceLock::new(),
            parent_glob_widening_sources: OnceLock::new(),
            module_wiring_unchanged,
            private_scope_wiring_unchanged,
            reexport_cargo_enabled,
            reexport_old_sources,
            reexport_source_modules: Mutex::new(HashMap::new()),
            reexport_module_files: Mutex::new(HashMap::new()),
            reexport_stable_chains: Mutex::new(HashMap::new()),
            files: Mutex::new(HashMap::new()),
            reexport_files: Mutex::new(HashMap::new()),
            leaf_sources: Mutex::new(HashMap::new()),
            visibility_modules: Mutex::new(HashMap::new()),
            cargo_configs: super::rust_cargo_config::CargoConfigProof::capture(root),
        }
    }

    pub(super) fn layout_cargo_settings_known(&self) -> bool {
        *self
            .layout_cargo_settings
            .get_or_init(|| self.scan_layout_cargo_settings().is_some())
    }

    /// 木の中の Cargo の設定は解析対象 (staged では index)、外側の設定は元の checkout から確かめる。
    fn scan_layout_cargo_settings(&self) -> Option<()> {
        let config = super::rust_cargo_config::CargoConfigProof::for_layout(&self.layout_checkout)?;
        let root = self.root.canonicalize().ok()?;
        let view = crate::git_support::tree_view(&root);
        for (path, kind) in view.omitted()? {
            // 省かれた設定や、設定ディレクトリを含み得るリンクは読めない。
            if !matches!(
                kind,
                crate::git_support::OmittedKind::Symlink(
                    crate::git_support::LinkTarget::RegularFile
                )
            ) || (!under_root_git_or_target(path)
                && path
                    .file_name()
                    .is_some_and(|name| name.eq_ignore_ascii_case("Cargo.toml")))
                || path
                    .parent()
                    .and_then(Path::file_name)
                    .is_some_and(|name| name.eq_ignore_ascii_case(".cargo"))
                    && path.file_name().is_some_and(|name| {
                        name.eq_ignore_ascii_case("config")
                            || name.eq_ignore_ascii_case("config.toml")
                    })
            {
                return None;
            }
        }
        if !self.skipped_cargo_inputs_known(false, |directory| {
            config.directory_keeps_sources(directory)
        }) {
            return None;
        }
        let mut pending = vec![root.clone()];
        while let Some(directory) = pending.pop() {
            for entry in std::fs::read_dir(&directory).ok()? {
                let entry = entry.ok()?;
                let name = entry.file_name();
                if directory == root && matches!(name.to_str(), Some("target" | ".git")) {
                    continue;
                }
                let path = entry.path();
                let kind = entry.file_type().ok()?;
                if name.eq_ignore_ascii_case(".cargo") && !config.directory_keeps_sources(&path) {
                    return None;
                }
                if kind.is_symlink() && !std::fs::metadata(&path).ok()?.is_file() {
                    return None;
                }
                if kind.is_dir() {
                    pending.push(path);
                } else if name.eq_ignore_ascii_case("Cargo.toml") {
                    let package = directory.strip_prefix(&root).ok()?;
                    let manifest = self.alias_cache.manifest(&root, package)?;
                    workspace_members::outside_unscanned(&root, package, &manifest)?;
                }
            }
        }
        Some(())
    }

    fn allows_module_paths(&self) -> bool {
        self.module_wiring_unchanged
            && self.layout_cargo_settings_known()
            && *self
                .ordinary_sources
                .get_or_init(|| scan_ordinary_sources(&self.root))
    }

    /// 変更元と参照元を、manifest と mod の宣言だけで通常の Cargo ソースのモジュールに結び付ける。
    /// `#[path]` や `include!` で別のモジュールにも入るファイルかは [`Self::unrelocated`] で
    /// 別に確かめる (プロジェクト全体の走査なので、除外を決める直前にだけ使う)。
    pub(super) fn ordinary_private_module(&self, file: &Path) -> Option<Arc<PrivateModule>> {
        if let Some(found) = self
            .private_modules
            .lock()
            .expect("private module cache poisoned")
            .get(file)
        {
            return found.clone();
        }
        let result = (|| {
            let package = file
                .ancestors()
                .find(|ancestor| self.root.join(ancestor).join("Cargo.toml").is_file())?;
            let manifest = self.alias_cache.manifest(&self.root, package)?;
            let metadata = manifest.get("package")?;
            if metadata.get("autolib").and_then(toml::Value::as_bool) == Some(false)
                || manifest
                    .get("lib")
                    .and_then(|lib| lib.get("path"))
                    .is_some()
            {
                return None;
            }
            // 入れ子の package では外側の通常の mod から内側のソースに到達し得る。
            for ancestor in package.ancestors().skip(1) {
                let manifest = self.root.join(ancestor).join("Cargo.toml");
                if manifest.exists() {
                    let value = self.alias_cache.manifest(&self.root, ancestor)?;
                    if value.get("package").is_some() || value.get("project").is_some() {
                        return None;
                    }
                }
            }
            let src = package.join("src");
            let root_name = file.strip_prefix(&src).ok()?;
            let mut modules = root_name
                .iter()
                .map(|part| part.to_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()?;
            let filename = modules.pop()?;
            let stem = filename.strip_suffix(".rs")?;
            if modules.first().is_some_and(|part| part == "bin")
                || modules.is_empty() && stem == "mod"
            {
                return None;
            }
            if stem != "mod" && (!modules.is_empty() || !matches!(stem, "lib" | "main")) {
                modules.push(stem.to_owned());
            }
            let library_only = self.module_roots(file, &src, &modules)?;
            Some(Arc::new(PrivateModule {
                src,
                path: modules,
                library_only,
            }))
        })();
        self.private_modules
            .lock()
            .expect("private module cache poisoned")
            .insert(file.to_owned(), result.clone());
        result
    }

    /// `file` が、ほかのソースの `#[path]` や `include!` で別のモジュールにも入り得ないか。
    pub(super) fn unrelocated(&self, file: &Path) -> bool {
        if let Some(&found) = self
            .relocation
            .lock()
            .expect("relocation cache poisoned")
            .get(file)
        {
            return found;
        }
        let result = self.static_layout(true).is_some_and(|relocated| {
            self.root
                .join(file)
                .canonicalize()
                .is_ok_and(|absolute| !relocated.iter().any(|path| absolute.starts_with(path)))
        });
        self.relocation
            .lock()
            .expect("relocation cache poisoned")
            .insert(file.to_owned(), result);
        result
    }

    pub(super) fn allows_collection_constructors(&self, file: &Path) -> bool {
        self.callable_layout_stable()
            && !self.reexport_old_sources.contains_key(file)
            && self.unrelocated(file)
            && self.alias_cache.collection_prelude(self, file).is_some()
            && self
                .root
                .join(file)
                .to_str()
                .is_some_and(|path| self.std_project.allows(path))
    }

    fn callable_layout_stable(&self) -> bool {
        self.callable_cargo_known
            && *self.callable_layout_stable.get_or_init(|| {
                self.reexport_old_sources.iter().all(|(path, old)| {
                    if path == Path::new("/dev/null") {
                        return true;
                    }
                    let Some(old) = old else {
                        return false;
                    };
                    let Ok(tree) = parser::parse_source(old, LangId::Rust) else {
                        return false;
                    };
                    !tree.root_node().has_error()
                        && !crate::engine::rust_dynamic_source::tree_includes_external_source(
                            tree.root_node(),
                            old,
                        )
                        && !crate::engine::rust_dynamic_source::any_rust_text_include(
                            tree.root_node(),
                            old,
                            |_| true,
                        )
                })
            })
    }

    /// 旧側にも別のモジュールへ現在のファイルを取り込む構文がないこと。
    /// 標準マクロへ式として渡すローカル名 `include` はソースの取り込みを生じない。
    pub(super) fn previous_receiver_layout_known(&self) -> bool {
        let Some(relocated) = self.static_layout(true) else {
            return false;
        };
        self.callable_cargo_known
            && self.reexport_old_sources.iter().all(|(file, old)| {
                if file == Path::new("/dev/null") {
                    return true;
                }
                let Some((source, path)) = old.as_deref().zip(file.to_str()) else {
                    return false;
                };
                let Ok(tree) = parser::parse_source(source, LangId::Rust) else {
                    return false;
                };
                let root = tree.root_node();
                let mut expressions = format_owner::StockExpressions::new(true);
                !root.has_error()
                    && !crate::engine::rust_dynamic_source::any_rust_text_include_at(
                        root,
                        source,
                        |node, literal| {
                            let call = match node.kind() {
                                "macro_invocation" => Some(node),
                                "identifier" => {
                                    node.parent().filter(|p| p.kind() == "macro_invocation")
                                }
                                _ => None,
                            };
                            literal.is_none()
                                || !call.is_some_and(|call| {
                                    expressions
                                        .proves(call, source, &self.root, path, self, relocated)
                                })
                        },
                    )
                    && !crate::engine::tree_walk::any_node(
                        root,
                        crate::engine::tree_walk::Children::Named,
                        |node| {
                            crate::engine::rust_dynamic_source::node_includes_external_source(
                                node, source,
                            ) && !(node.kind() == "identifier"
                                && expressions
                                    .proves(node, source, &self.root, path, self, relocated))
                        },
                    )
            })
    }

    pub(super) fn declared_module_file(&self, src: &Path, module: &[String]) -> Option<PathBuf> {
        let key = (src.to_owned(), module.to_vec());
        if let Some(cached) = self
            .ordinary_files
            .lock()
            .expect("module cache poisoned")
            .get(&key)
        {
            return cached.clone();
        }
        let path = enum_owner::ordinary_module_file(&self.root, src, module);
        self.ordinary_files
            .lock()
            .expect("module cache poisoned")
            .insert(key, path.clone());
        path
    }

    pub(super) fn macro_tokens_stay_in_module(&self) -> bool {
        self.static_layout(true).is_some() && self.macros_may_export_tokens.get() == Some(&false)
    }

    pub(super) fn outside_parent(&self, file: &Path, reader: &Path) -> bool {
        let Some(module) = self.ordinary_private_module(file) else {
            return false;
        };
        let Some((_, parent)) = module.path.split_last() else {
            return false;
        };
        let Some(package) = module.src.parent() else {
            return false;
        };
        let directory = module.src.join(parent.iter().collect::<PathBuf>());
        self.callable_layout_stable()
            && self
                .declared_module_file(&module.src, &module.path)
                .as_deref()
                == Some(file)
            && reader.starts_with(package)
            && !reader.starts_with(&directory)
            && reader != directory.with_extension("rs")
            && self.unrelocated(file)
            && self.unrelocated(reader)
    }

    /// 差分の Rust ソース (変更元と、[`Self::owner_source`] が許した本体だけの変更) のどれも、
    /// 別のモジュールに入り得ないか。
    pub(super) fn changed_sources_unrelocated(&self) -> bool {
        *self.changed_sources_unrelocated.get_or_init(|| {
            self.reexport_old_sources
                .keys()
                .all(|path| self.unrelocated(path))
        })
    }

    /// lib と main の最初の mod が同じファイルを指したら、以降の宣言列は共有する。
    fn module_roots(&self, file: &Path, src: &Path, modules: &[String]) -> Option<bool> {
        let lib = src.join("lib.rs");
        let main = src.join("main.rs");
        let declares = |path: &Path, name: &str| -> Option<bool> {
            if !checked_is_file(&self.root, path)? {
                return Some(false);
            }
            match self
                .alias_cache
                .module(&self.root, path)?
                .external_modules
                .get(name)
            {
                None => Some(false),
                Some(1) => Some(true),
                _ => None,
            }
        };
        let Some(first) = modules.first() else {
            let name = file.file_stem()?.to_str()?;
            if declares(&lib, name)? || declares(&main, name)? {
                return None;
            }
            return Some(file == lib);
        };
        let library = declares(&lib, first)?;
        let binary = declares(&main, first)?;
        if !library && !binary {
            return None;
        }
        let mut directory = src.to_owned();
        let mut parent: Option<PathBuf> = None;
        for module in modules {
            if let Some(path) = &parent
                && !declares(path, module)?
            {
                return None;
            }
            directory.push(module);
            let normal = directory.with_extension("rs");
            let nested = directory.join("mod.rs");
            parent = Some(
                match (
                    checked_is_file(&self.root, &normal)?,
                    checked_is_file(&self.root, &nested)?,
                ) {
                    (true, false) => normal,
                    (false, true) => nested,
                    _ => return None,
                },
            );
        }
        (parent.as_deref() == Some(file)).then_some(library && !binary)
    }

    pub(super) fn owner_source(&self, ctx: &super::FileContext) -> Option<Arc<PrivateModule>> {
        let file = Path::new(&ctx.new_path);
        if !self.reexport_cargo_enabled
            || (ctx.rust_inherent_method_owners.is_empty()
                && ctx.rust_private_types.is_empty()
                && ctx.rust_private_root_functions.is_empty())
        {
            return None;
        }
        if self.reexport_old_sources.len() > 1
            && self
                .owner_changes
                .get_or_init(|| {
                    self.reexport_old_sources
                        .iter()
                        .filter(|(path, old)| {
                            !old.as_deref()
                                .zip(read_input_file(&self.root.join(path)))
                                .is_some_and(|(old, new)| {
                                    super::rust_owner_alias::plain_function_body_edit(old, &new)
                                        && self.ordinary_private_module(path).is_some()
                                })
                        })
                        .map(|(p, _)| p.clone())
                        .collect()
                })
                .iter()
                .any(|changed| changed != file)
        {
            return None;
        }
        if ctx.rust_owner_derive && !self.private_scope_wiring_unchanged {
            return None;
        }
        self.ordinary_private_module(file)
    }

    /// 除外の直前に確かめる配置 ([`Self::changed_sources_unrelocated`]) か derive
    /// ([`Self::owner_derive_proved`]) が、すでに失敗と分かっているか。分かっていれば、以降の参照
    /// ファイルで呼び出しの索引を作らない (まだ確かめていないものは確かめない)。
    pub(super) fn owner_proof_refuted(&self, ctx: &super::FileContext) -> bool {
        self.changed_sources_unrelocated.get() == Some(&false)
            || (ctx.rust_owner_derive && self.owner_derive_safe.get() == Some(&false))
    }

    /// package (`from`・`to` は package のディレクトリ、root 相対) の path の依存 (通常・開発・build と
    /// target 別、`workspace = true` の継承を含む) の閉包が `to` を含み得るか。registry の依存は lock で
    /// 固定した別の写しで、作業ツリーの変更が届かないのでたどらない。git の依存は、その先の manifest の
    /// `path` (絶対パスなど) が作業ツリーへ戻り得るので決めない。
    ///
    /// Cargo が依存の取得元を差し替え得る (木のどこかの manifest の `[patch]`・`[replace]`、木の
    /// どこか (Cargo を起動し得るディレクトリ) と祖先と `$CARGO_HOME` の設定、`$CARGO_HOME` と木の
    /// どちらかがもう一方の中にある)、たどる package の workspace が root でない、root の外を指す path や
    /// `base` がある、git の依存がある、manifest を読めない・形が違うときは `None`。
    pub(super) fn package_reaches(&self, from: &Path, to: &Path) -> Option<bool> {
        if !*self
            .dependency_sources_fixed
            .get_or_init(|| self.scan_dependency_sources())
        {
            return None;
        }
        let closure = {
            let cached = self
                .dependency_closures
                .lock()
                .expect("dependency closure cache poisoned")
                .get(from)
                .cloned();
            match cached {
                Some(closure) => closure,
                None => {
                    let closure = self.dependency_closure(from).map(Arc::new);
                    self.dependency_closures
                        .lock()
                        .expect("dependency closure cache poisoned")
                        .insert(from.to_owned(), closure.clone());
                    closure
                }
            }
        }?;
        Some(closure.packages.contains(&self.package_identity(to)?))
    }

    /// `directory` (root 相対) の package が proc-macro のクレート (`[lib] proc-macro = true` か、
    /// `crate-type` に `proc-macro`) か。manifest を読めなければ `None`。
    pub(super) fn is_proc_macro_package(&self, directory: &Path) -> Option<bool> {
        let manifest = self.alias_cache.manifest(&self.root, directory)?;
        let lib = manifest.get("lib");
        Some(
            lib.and_then(|lib| lib.get("proc-macro").or_else(|| lib.get("proc_macro")))
                .is_some_and(|value| value.as_bool() != Some(false))
                || lib
                    .and_then(|lib| lib.get("crate-type").or_else(|| lib.get("crate_type")))
                    .is_some_and(|types| {
                        types.as_array().is_none_or(|types| {
                            types
                                .iter()
                                .any(|kind| kind.as_str().is_none_or(|kind| kind == "proc-macro"))
                        })
                    }),
        )
    }

    /// `directory` (root 相対) の package が、edition 2018 以降で、registry の `objc2` の 0.6
    /// (`define_class!` のメソッドの本体を、呼び出したモジュールに生成するメソッドで評価する版) に依存し、
    /// 木のどこにも `define_class`・`objc2` を束縛し直す綴りが無いか
    /// ([`super::rust_macro_site::rebinds_objc2_names`])。名前を付け替えた依存 (`package = ..`)・path・
    /// git・別の registry の `objc2`、読めない manifest は `None`。edition 2018 以降の `use objc2::..` の
    /// `objc2` は、同じ名前のローカルの束縛 (glob の import を含む) があれば曖昧さのエラーになる。
    pub(super) fn objc2_define_class(&self, directory: &Path) -> Option<bool> {
        if !self.edition_2018_or_later(directory)? {
            return None;
        }
        let manifest = self.alias_cache.manifest(&self.root, directory)?;
        // 通常の依存の表だけを見る (`[build-dependencies]`・`[dev-dependencies]` の `objc2` は
        // ライブラリ・バイナリに渡らず、`use objc2::..` が曖昧さのエラーにならずに別の束縛を指し得る)。
        let mut tables = Vec::new();
        tables.extend(manifest.get("dependencies"));
        if let Some(targets) = manifest.get("target") {
            for platform in targets.as_table()?.values() {
                tables.extend(platform.as_table()?.get("dependencies"));
            }
        }
        let mut found = false;
        for table in tables {
            for (name, spec) in table.as_table()? {
                let renamed_to_objc2 = spec
                    .get("package")
                    .and_then(toml::Value::as_str)
                    .is_some_and(|package| package == "objc2");
                if name != "objc2" && !renamed_to_objc2 {
                    continue;
                }
                let version = match spec {
                    toml::Value::String(version) => version.as_str(),
                    toml::Value::Table(fields)
                        if !renamed_to_objc2
                            && [
                                "path",
                                "git",
                                "registry",
                                "registry-index",
                                "base",
                                "workspace",
                                "package",
                            ]
                            .iter()
                            .all(|key| !fields.contains_key(*key))
                            // `optional = false` は常に有効な通常の依存。
                            && fields
                                .get("optional")
                                .is_none_or(|optional| optional.as_bool() == Some(false)) =>
                    {
                        fields.get("version")?.as_str()?
                    }
                    _ => return None,
                };
                let version = version.trim().trim_start_matches(['^', '=', '~']).trim();
                let minor = version.strip_prefix("0.6")?;
                if !(minor.is_empty() || minor.starts_with('.')) {
                    return None;
                }
                found = true;
            }
        }
        Some(
            found
                && !*self
                    .in_tree_define_class
                    .get_or_init(|| self.defines_define_class()),
        )
    }

    /// `directory` の package の edition が 2018 以降か。`edition.workspace = true` は、root の workspace の
    /// member なら root の `[workspace.package]` から読む。書いていない (2015) ・読めないときは `None`。
    /// target ごとの `edition` (`[lib] edition = "2015"`) も 2018 以降であることを求める。
    fn edition_2018_or_later(&self, directory: &Path) -> Option<bool> {
        let manifest = self.alias_cache.manifest(&self.root, directory)?;
        for kind in ["lib", "bin", "example", "test", "bench"] {
            let targets = match manifest.get(kind) {
                None => continue,
                Some(toml::Value::Array(targets)) => targets.iter().collect::<Vec<_>>(),
                Some(target) => vec![target],
            };
            for target in targets {
                if let Some(edition) = target.get("edition")
                    && !matches!(edition.as_str(), Some("2018" | "2021" | "2024"))
                {
                    return None;
                }
            }
        }
        let edition = match manifest.get("package")?.get("edition")? {
            toml::Value::String(edition) => edition.clone(),
            toml::Value::Table(fields)
                if fields.len() == 1
                    && fields.get("workspace").and_then(toml::Value::as_bool) == Some(true) =>
            {
                if !self.in_root_workspace(directory)? {
                    return None;
                }
                let root = self.alias_cache.manifest(&self.root, Path::new(""))?;
                root.get("workspace")?
                    .get("package")?
                    .get("edition")?
                    .as_str()?
                    .to_owned()
            }
            _ => return None,
        };
        Some(matches!(edition.as_str(), "2018" | "2021" | "2024"))
    }

    /// 木の中 (root の直下の `.git`・`target` を除く) のどれかの `.rs` に、`define_class`・`objc2` を
    /// 束縛し直し得る綴りがあるか ([`super::rust_macro_site::rebinds_objc2_names`]。どちらかの単語を
    /// 含むファイルだけを構文解析する)。`#[macro_use]`・`#[no_implicit_prelude]` の属性があれば真
    /// ([`super::rust_macro_site::changes_name_resolution`])。読めなければ真。
    ///
    /// 走査が読まないソースがあり得るときも真: symlink のディレクトリ、木の中の proc-macro のクレート
    /// (文字列から束縛・属性を作り得る)、明示の target が `.rs` でない・root の直下の `.git`・`target`
    /// にある・木の外にある package、`include!`・`#[path]` が字面のパスで木の中の `.rs` を指さない
    /// もの (`.rs` でないファイル・生成物の `include!(concat!(env!("OUT_DIR"), ..))` など)、`include!` で
    /// 読む `.rs` の中の `#[path]` (取り込んだモジュールから解くので、ファイルの場所から決められない)。
    fn defines_define_class(&self) -> bool {
        let mut pending = vec![self.root.clone()];
        while let Some(directory) = pending.pop() {
            let Ok(entries) = std::fs::read_dir(&directory) else {
                return true;
            };
            for entry in entries {
                let Ok(entry) = entry else {
                    return true;
                };
                let Ok(kind) = entry.file_type() else {
                    return true;
                };
                let path = entry.path();
                // symlink のディレクトリは辿らない (中の `.rs` を読めない)。
                if kind.is_symlink()
                    && std::fs::metadata(&path).map_or(true, |metadata| metadata.is_dir())
                {
                    return true;
                }
                if kind.is_dir() {
                    if directory == self.root
                        && matches!(entry.file_name().to_str(), Some(".git" | "target"))
                    {
                        continue;
                    }
                    pending.push(path);
                } else if entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.eq_ignore_ascii_case("Cargo.toml"))
                {
                    let Ok(package) = directory.strip_prefix(&self.root) else {
                        return true;
                    };
                    if !self.package_sources_scanned(package) {
                        return true;
                    }
                } else if path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("rs"))
                    && self.source_may_rebind_objc2(&path)
                {
                    return true;
                }
            }
        }
        false
    }

    /// `package` (root 相対) の package のソースを、[`Self::defines_define_class`] の走査が読むか:
    /// proc-macro のクレートでなく、明示の target がどれも木の中 (root の直下の `.git`・`target` を除く)
    /// の `.rs`。読めない manifest は偽。
    fn package_sources_scanned(&self, package: &Path) -> bool {
        if self.is_proc_macro_package(package) != Some(false) {
            return false;
        }
        let Some(manifest) = self.alias_cache.manifest(&self.root, package) else {
            return false;
        };
        let Some(targets) = RustTargets::from_manifest(&manifest, package) else {
            return false;
        };
        targets.roots_complete
            && targets.explicit_roots().all(|target| {
                !under_root_git_or_target(target)
                    && target
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("rs"))
            })
    }

    /// 木の中の `.rs` の `file` に、`define_class`・`objc2` を束縛し直し得る綴り・名前の解決を変える
    /// 属性・走査が読まないソースの取り込みがあるか ([`Self::defines_define_class`])。読めなければ真。
    fn source_may_rebind_objc2(&self, file: &Path) -> bool {
        let Some(source) = read_input_file(file) else {
            return true;
        };
        let attributes = memchr::memmem::find(&source, b"macro_use").is_some()
            || memchr::memmem::find(&source, b"no_implicit_prelude").is_some();
        let names = memchr::memmem::find(&source, b"define_class").is_some()
            || memchr::memmem::find(&source, b"objc2").is_some();
        let sources = memchr::memmem::find(&source, b"include").is_some()
            || memchr::memmem::find(&source, b"path").is_some();
        if !attributes && !names && !sources {
            return false;
        }
        let Ok(tree) = parser::parse_source(&source, LangId::Rust) else {
            return true;
        };
        let root = tree.root_node();
        if (attributes && super::rust_macro_site::changes_name_resolution(root, &source))
            || (names && super::rust_macro_site::rebinds_objc2_names(root, &source))
        {
            return true;
        }
        if !sources {
            return false;
        }
        let Some(directory) = file.strip_prefix(&self.root).ok().and_then(Path::parent) else {
            return true;
        };
        crate::engine::tree_walk::walk_preorder(
            root,
            crate::engine::tree_walk::Children::All,
            |node, _| {
                if !crate::engine::rust_dynamic_source::node_includes_external_source(node, &source)
                {
                    return std::ops::ControlFlow::Continue(
                        crate::engine::tree_walk::Walk::Descend,
                    );
                }
                // 字面のパスで木の中の `.rs` を指す `include!("..")`・`#[path = ".."]` だけを許す。
                let Some((include, literal)) = literal_source_path(node, &source) else {
                    return std::ops::ControlFlow::Break(());
                };
                let Some(target) = lexical_join(directory, Path::new(&literal)) else {
                    return std::ops::ControlFlow::Break(());
                };
                if under_root_git_or_target(&target)
                    || !target
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("rs"))
                    || (include && included_file_has_path_attribute(&self.root.join(&target)))
                {
                    return std::ops::ControlFlow::Break(());
                }
                std::ops::ControlFlow::Continue(crate::engine::tree_walk::Walk::Skip)
            },
        )
        .is_break()
    }

    fn package_identity(&self, directory: &Path) -> Option<crate::git_support::EntryId> {
        crate::git_support::open_entry_id(&self.root.join(directory))
    }

    /// `directory` (root 相対の package) の workspace が root の manifest か (Cargo は package から
    /// 祖先へ `[workspace]` を探し、`exclude` された workspace は飛ばす。`exclude` は manifest のパスと
    /// 照合するので、`e/Cargo.toml` も `e` を除く)。`package.workspace` は、root を指せば root の
    /// member (member でなければ Cargo が拒む)。root に `[workspace]` が無い、`package.workspace` が
    /// root 以外を指す、manifest を読めないときは `None`。
    fn in_root_workspace(&self, directory: &Path) -> Option<bool> {
        let manifest = self.alias_cache.manifest(&self.root, directory)?;
        if let Some(pointer) = manifest
            .get("package")
            .and_then(|package| package.get("workspace"))
        {
            let workspace = lexical_join(directory, Path::new(pointer.as_str()?))?;
            return workspace.as_os_str().is_empty().then_some(true);
        }
        if directory.as_os_str().is_empty() {
            return Some(true);
        }
        let workspace = self.alias_cache.manifest(&self.root, Path::new(""))?;
        let workspace = workspace.get("workspace")?;
        for ancestor in directory.ancestors() {
            if ancestor.as_os_str().is_empty() {
                break;
            }
            if self.root.join(ancestor).join("Cargo.toml").exists()
                && self
                    .alias_cache
                    .manifest(&self.root, ancestor)?
                    .get("workspace")
                    .is_some()
            {
                return Some(false);
            }
        }
        if let Some(excluded) = workspace.get("exclude") {
            for path in excluded.as_array()? {
                let excluded = lexical_join(Path::new(""), Path::new(path.as_str()?))?;
                if directory.join("Cargo.toml").starts_with(&excluded) {
                    return Some(false);
                }
            }
        }
        Some(true)
    }

    /// 木の中の manifest がどれも `[patch]`・`[replace]` を持たず、木の中の `.cargo` の設定が取得元を
    /// 差し替えず、`$CARGO_HOME` と木のどちらももう一方の中に無いか (root の直下の `.git`・`target` は
    /// 読まない。symlink のディレクトリは中を確かめられないので偽)。Cargo はどのディレクトリからも
    /// 起動され得る。木が `$CARGO_HOME` の中 (git の依存の checkout) なら、git の依存がこの木を指し得る。
    fn scan_dependency_sources(&self) -> bool {
        let Ok(root) = self.root.canonicalize() else {
            return false;
        };
        let Some(home) = self.cargo_configs.home() else {
            return false;
        };
        let home = home.canonicalize().unwrap_or_else(|_| home.to_owned());
        if home.starts_with(&root) || root.starts_with(&home) {
            return false;
        }
        // root の直下の `.git`・`target` は配置の走査が読まないが、その下のディレクトリから Cargo を
        // 起動すると、そこの `.cargo` の設定を読む (Cargo の作るディレクトリと Git の object は除く)。
        if !self.skipped_cargo_inputs_known(true, |directory| {
            directory
                .parent()
                .is_some_and(|parent| self.cargo_configs.keeps_dependency_sources(parent))
        }) {
            return false;
        }
        let mut pending = vec![PathBuf::new()];
        while let Some(directory) = pending.pop() {
            let Ok(entries) = std::fs::read_dir(self.root.join(&directory)) else {
                return false;
            };
            for entry in entries {
                let Ok(entry) = entry else {
                    return false;
                };
                let Ok(kind) = entry.file_type() else {
                    return false;
                };
                let name = entry.file_name();
                let path = directory.join(&name);
                if kind.is_dir() {
                    if directory.as_os_str().is_empty()
                        && matches!(name.to_str(), Some(".git" | "target"))
                    {
                        continue;
                    }
                    if name
                        .to_str()
                        .is_none_or(|name| name.eq_ignore_ascii_case(".cargo"))
                        && !self
                            .cargo_configs
                            .keeps_dependency_sources(&self.root.join(&directory))
                    {
                        return false;
                    }
                    pending.push(path);
                } else if kind.is_symlink() {
                    if std::fs::metadata(self.root.join(&path)).is_ok_and(|meta| meta.is_dir()) {
                        return false;
                    }
                } else if name
                    .to_str()
                    .is_some_and(|name| name.eq_ignore_ascii_case("Cargo.toml"))
                {
                    let Some(manifest) = self.alias_cache.manifest(&self.root, &directory) else {
                        return false;
                    };
                    // root の直下の `target`・`.git` を指す path の依存は、そこの package を workspace
                    // の member にする (配置の走査が読まない package が別のクレートのファイルを取り込み得る)。
                    if manifest.get("patch").is_some() || manifest.get("replace").is_some() {
                        return false;
                    }
                    let Some(dependencies) =
                        manifest_path_dependencies(&root, &manifest, &directory)
                    else {
                        return false;
                    };
                    if dependencies
                        .iter()
                        .flatten()
                        .any(|path| under_root_git_or_target(path))
                    {
                        return false;
                    }
                }
            }
        }
        true
    }

    fn skipped_cargo_inputs_known(
        &self,
        reject_packages: bool,
        check_config: impl Fn(&Path) -> bool,
    ) -> bool {
        self.unscanned_cargo_configs_keep_sources(".git", &[], reject_packages, &check_config)
            && self.unscanned_cargo_configs_keep_sources(
                "target",
                &CARGO_OUTPUTS,
                reject_packages,
                &check_config,
            )
    }

    /// root の直下の `top` (`target`・`.git`) の下の `.cargo` の設定が取得元を差し替えないか。Cargo と
    /// Git が作るディレクトリは読まない: `top` の直下の `skipped_children`、Cargo の profile
    /// (`.cargo-lock` を持つ) の中の `PROFILE_OUTPUTS`、Git のディレクトリ ([`git_directory_shape`]) の
    /// 中の `objects`・`lfs`。symlink のディレクトリと、たどる要素が多すぎるときは偽。
    fn unscanned_cargo_configs_keep_sources(
        &self,
        top: &str,
        skipped_children: &[&str],
        reject_packages: bool,
        check_config: &impl Fn(&Path) -> bool,
    ) -> bool {
        const PROFILE_OUTPUTS: [&str; 5] =
            ["build", "deps", "incremental", ".fingerprint", "examples"];
        let mut pending = vec![PathBuf::from(top)];
        let mut budget = 100_000usize;
        while let Some(directory) = pending.pop() {
            let full = self.root.join(&directory);
            // Cargo の profile の `.cargo-lock` はファイル (同じ名前のディレクトリは目印にしない)。
            let profile = full.join(".cargo-lock").is_file();
            let git_directory = git_directory_shape(&full);
            let entries = match std::fs::read_dir(&full) {
                Ok(entries) => entries,
                // 無いもの (`target` を作る前)・ディレクトリでないもの (worktree の `.git` のファイル) の
                // 中に設定は無い。
                Err(_)
                    if std::fs::metadata(&full).map_or_else(
                        |error| error.kind() == std::io::ErrorKind::NotFound,
                        |metadata| !metadata.is_dir(),
                    ) =>
                {
                    continue;
                }
                // 列挙できないディレクトリ (検索だけ許すもの) の中の設定は、既知の名前で開け得る。
                Err(_) => return false,
            };
            for entry in entries {
                let Some(left) = budget.checked_sub(1) else {
                    return false;
                };
                budget = left;
                let Ok(entry) = entry else {
                    return false;
                };
                let Ok(kind) = entry.file_type() else {
                    return false;
                };
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    return false;
                };
                if name.eq_ignore_ascii_case(".cargo") {
                    if !check_config(&entry.path()) {
                        return false;
                    }
                    // `.cargo` の中のディレクトリから起動した Cargo も、その下の `.cargo` を読む。
                    pending.push(directory.join(name));
                    continue;
                }
                // 走査が読まないディレクトリの manifest・lockfile は、走査が読まない package や別の
                // workspace (別の lockfile の解決) になり得る (Cargo の profile の直下にも無い)。
                // Cargo は symlink の manifest もたどって読む。
                if reject_packages
                    && !kind.is_dir()
                    && (name.eq_ignore_ascii_case("Cargo.toml")
                        || name.eq_ignore_ascii_case("Cargo.lock"))
                {
                    return false;
                }
                if (profile && PROFILE_OUTPUTS.contains(&name))
                    || (git_directory && matches!(name, "objects" | "lfs"))
                    || (directory.as_os_str() == top && skipped_children.contains(&name))
                {
                    continue;
                }
                if kind.is_symlink() {
                    if std::fs::metadata(entry.path()).is_ok_and(|meta| meta.is_dir()) {
                        return false;
                    }
                    continue;
                }
                if kind.is_dir() {
                    pending.push(directory.join(name));
                }
            }
        }
        true
    }

    /// [`Self::package_reaches`] の、`from` から path でたどれる package (自身を含む) の実体と、
    /// たどった package の registry・git の依存の package 名。
    fn dependency_closure(&self, from: &Path) -> Option<DependencyClosure> {
        if !self
            .cargo_configs
            .keeps_dependency_sources(&self.root.join(from))
        {
            return None;
        }
        let workspace = self.alias_cache.manifest(&self.root, Path::new(""))?;
        let mut pending = vec![from.to_owned()];
        let mut closure = DependencyClosure::default();
        while let Some(directory) = pending.pop() {
            if !closure.packages.insert(self.package_identity(&directory)?) {
                continue;
            }
            if !self.in_root_workspace(&directory)? {
                return None;
            }
            let manifest = self.alias_cache.manifest(&self.root, &directory)?;
            let mut tables = Vec::new();
            for kind in DEPENDENCY_TABLES {
                tables.extend(manifest.get(kind));
            }
            if let Some(targets) = manifest.get("target") {
                for platform in targets.as_table()?.values() {
                    for kind in DEPENDENCY_TABLES {
                        tables.extend(platform.as_table()?.get(kind));
                    }
                }
            }
            for table in tables {
                for (name, spec) in table.as_table()? {
                    let mut names = vec![name.as_str()];
                    let (base, spec) = match spec {
                        toml::Value::String(_) => (directory.clone(), spec),
                        toml::Value::Table(fields) => {
                            if fields.get("base").is_some() {
                                return None;
                            }
                            names.extend(fields.get("package").and_then(toml::Value::as_str));
                            if fields.get("workspace").and_then(toml::Value::as_bool) == Some(true)
                            {
                                let inherited = workspace
                                    .get("workspace")?
                                    .get("dependencies")?
                                    .get(name.as_str())?;
                                (PathBuf::new(), inherited)
                            } else {
                                (directory.clone(), spec)
                            }
                        }
                        _ => return None,
                    };
                    let path = match spec {
                        toml::Value::String(_) => None,
                        toml::Value::Table(fields) => {
                            if fields.get("base").is_some() {
                                return None;
                            }
                            names.extend(fields.get("package").and_then(toml::Value::as_str));
                            fields.get("path")
                        }
                        _ => return None,
                    };
                    let Some(path) = path else {
                        // registry の依存は lock で固定した写しで、作業ツリーの変更は届かない。git の
                        // 依存は、その先の manifest の `path` が作業ツリーへ戻り得るので決めない。
                        if spec.get("git").is_some() {
                            return None;
                        }
                        continue;
                    };
                    let joined = lexical_join(&base, Path::new(path.as_str()?))?;
                    // root の直下の `target`・`.git` は配置の走査が読まないので、そこの package は
                    // 別のクレートのファイルを取り込み得る。
                    if under_root_git_or_target(&joined) {
                        return None;
                    }
                    pending.push(joined);
                }
            }
        }
        Some(closure)
    }

    /// 変更元の `#[derive(Debug)]` を、どのソースも import・マクロ定義・外部の glob で差し替えて
    /// いないか (derive が無ければ真)。プロジェクト全体の走査なので、除外を決める直前にだけ使う。
    pub(super) fn owner_derive_proved(&self, ctx: &super::FileContext) -> bool {
        !ctx.rust_owner_derive
            || *self
                .owner_derive_safe
                .get_or_init(|| static_placement_bindings(&self.root, false).is_some())
    }

    /// プロジェクトのソースの配置 (配置を変え得るディレクトリの一覧)。`literal_includes` なら
    /// 文字列リテラルの `include!` を、取り込み先と元のディレクトリを保留して許す。
    ///
    /// 2 つの走査は `include!` を見つけたときだけ分かれる。許す側の結果で 1 つも受け入れて
    /// いなければ、許さない側も同じ一覧になり、受け入れていれば許さない側は `None` になる。
    /// 許さない側の結果が `Some` なら `include!` は無く、許す側も同じ一覧になる。先に計算した側の
    /// 結果から導けるときは、プロジェクト全体の走査をやり直さない。
    fn static_layout(&self, literal_includes: bool) -> Option<&Vec<PathBuf>> {
        if literal_includes {
            if let Some(Some(plain)) = self.static_source_layout.get() {
                return Some(plain);
            }
            return self
                .private_layout
                .get_or_init(|| enum_owner::static_layout(&self.root, self, true))
                .as_ref()
                .map(|(relocated, _, _)| relocated);
        }
        if let Some(layout) = self.private_layout.get() {
            return match layout {
                Some((relocated, false, _)) => Some(relocated),
                _ => None,
            };
        }
        self.static_source_layout
            .get_or_init(|| {
                enum_owner::static_layout(&self.root, self, false)
                    .map(|(relocated, _, _)| relocated)
            })
            .as_ref()
    }

    /// 木の中に `macro_rules!` の定義 (マクロの本体・引数の中の `macro_rules` も) と、走査で読み
    /// 切れない入力 (先頭行の `#! [..]`・root の外を指す path の依存) が無く、配置を決められたか
    /// ([`enum_owner::static_layout`])。木の外のマクロは `include!`・`#[path]`・子の `mod` を
    /// 組み立てない (前提) ので、これが真なら、字句を組み立てて別のモジュールへ取り込む経路は無い。
    pub(super) fn macro_free(&self) -> bool {
        self.private_layout
            .get_or_init(|| enum_owner::static_layout(&self.root, self, true))
            .as_ref()
            .is_some_and(|(_, _, macros)| !macros)
    }

    fn allows_static_source_path(&self, path: &Path) -> bool {
        let Some(relocated) = self.static_layout(false) else {
            return false;
        };
        let Some(file) = self.root.join(path).canonicalize().ok() else {
            return false;
        };
        !relocated
            .iter()
            .any(|directory| file.starts_with(directory))
    }

    fn allows_private_scope_paths(&self) -> bool {
        self.private_scope_wiring_unchanged
            && self.layout_cargo_settings_known()
            && self.has_safe_private_import_environment()
            && (*self
                .private_scope_sources
                .get_or_init(|| scan_private_scope_sources(&self.root))
                || self.allows_reexport_paths())
    }

    pub(super) fn allows_reexport_paths(&self) -> bool {
        self.reexport_cargo_enabled
            && self.layout_cargo_settings_known()
            && *self
                .reexport_sources
                .get_or_init(|| scan_reexport_sources(&self.root))
    }

    pub(super) fn allows_external_alias_paths(&self) -> bool {
        self.reexport_cargo_enabled
    }

    /// 非公開の親 glob だけなら、名前の輸送がないことを確かめて可視性の証明を使う。
    pub(super) fn allows_private_widening_paths(&self, name: &str) -> bool {
        if !self.allows_private_scope_paths() {
            return false;
        }
        if *self
            .no_glob_imports
            .get_or_init(|| scan_sources(&self.root, super::rust_visibility_scope::has_glob_import))
        {
            return true;
        }
        let name = name.strip_prefix("r#").unwrap_or(name);
        if name.is_empty() || !name.is_ascii() {
            return false;
        }
        self.widening_glob_imports
            .get_or_init(|| {
                let mut names = HashSet::new();
                let safe = scan_sources(&self.root, |source| {
                    let Some(imports) =
                        super::rust_visibility_scope::parent_glob_import_names(source)
                    else {
                        return true;
                    };
                    names.extend(imports);
                    false
                });
                safe.then_some(names)
            })
            .as_ref()
            .is_some_and(|imports| !imports.contains(name))
    }

    /// 通常の直下子 module は、親の private 宣言を変更前から参照できる。
    pub(super) fn private_widening_direct_child(&self, source_path: &str, ref_path: &str) -> bool {
        if !self.private_scope_wiring_unchanged
            || !*self.parent_glob_widening_sources.get_or_init(|| {
                scan_safe_source_environment(&self.root, |source| {
                    source_can_retarget_private_scope(source)
                        || super::rust_visibility_scope::has_non_parent_glob(source)
                })
            })
        {
            return false;
        }
        let source = Path::new(source_path);
        let Some(filename) = source.file_name().and_then(|name| name.to_str()) else {
            return false;
        };
        let base = match filename {
            // crate root と通常の module の両方になり得る名前は既存の Cargo 証明に任せる。
            "lib.rs" | "main.rs" => return false,
            "mod.rs" => source.parent().unwrap_or(Path::new("")).to_owned(),
            _ => source.with_extension(""),
        };
        let alternative = if filename == "mod.rs" {
            base.with_extension("rs")
        } else {
            base.join("mod.rs")
        };
        if crate::git_support::tree_view(&self.root).hides(&alternative)
            || self.root.join(alternative).exists()
        {
            return false;
        }
        let reference = Path::new(ref_path);
        let Ok(relative) = reference.strip_prefix(&base) else {
            return false;
        };
        let parts = relative.components().collect::<Vec<_>>();
        let name = match parts.as_slice() {
            [std::path::Component::Normal(file)] => {
                file.to_str().and_then(|name| name.strip_suffix(".rs"))
            }
            [
                std::path::Component::Normal(module),
                std::path::Component::Normal(file),
            ] if *file == "mod.rs" => module.to_str(),
            _ => None,
        };
        let Some(name) = name.filter(|name| {
            !name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        }) else {
            return false;
        };
        let file = base.join(format!("{name}.rs"));
        let directory_file = base.join(name).join("mod.rs");
        let (Some(file_exists), Some(directory_file_exists)) = (
            checked_is_file(&self.root, &file),
            checked_is_file(&self.root, &directory_file),
        ) else {
            return false;
        };
        if file_exists == directory_file_exists {
            return false;
        }
        let Some(Some(old)) = self.reexport_old_sources.get(source) else {
            return false;
        };
        let Ok(tree) = parser::parse_source(old, LangId::Rust) else {
            return false;
        };
        let root = tree.root_node();
        if root.has_error() {
            return false;
        }
        let mut cursor = root.walk();
        let mut declarations = root.named_children(&mut cursor).filter(|node| {
            node.kind() == "mod_item"
                && node
                    .child_by_field_name("name")
                    .and_then(|node| node.utf8_text(old).ok())
                    .is_some_and(|value| value.strip_prefix("r#").unwrap_or(value) == name)
        });
        declarations
            .next()
            .is_some_and(|node| node.child_by_field_name("body").is_none())
            && declarations.next().is_none()
    }

    /// 可視性だけの拡大で、参照ファイルが宣言の旧範囲・新しい範囲のどこに入るか。
    /// ゲートが閉じている、宣言と参照の所属のモジュールを証明できない、別の crate に属する、
    /// ファイル内の書き換えが拡大でない場合は `None` (この証明では判定を変えない)。
    pub(super) fn explicit_widening_scope(
        &self,
        source_path: &str,
        ref_path: &str,
        name: &str,
        proof: &super::rust_visibility_scope::ExplicitWidening,
    ) -> Option<super::rust_visibility_scope::WideningScope> {
        if !self.allows_private_widening_paths(name) {
            return None;
        }
        let source = self.visibility_module(Path::new(source_path))?;
        let reference = self.visibility_module(Path::new(ref_path))?;
        if source.crate_root != reference.crate_root {
            return None;
        }
        proof.classify(&source.modules, &reference.modules)
    }

    /// 参照を昇格する前に変更前後の mod / use / 属性を同じソースの写しで比べる。
    pub(super) fn private_call_project(&self) -> Option<&Self> {
        let Some(input) = self.staged_binding_diff.as_deref() else {
            return self.private_scope_wiring_unchanged.then_some(self);
        };
        let view = self.staged_bindings()?;
        let verified = self.staged_private_call_wiring.get_or_init(|| {
            let files = crate::engine::diff::parse_unified_diff(input);
            super::rust_cargo_inputs_unchanged(
                &crate::engine::diff::scan_blocks(input),
                &files,
                false,
            ) && private_scope_wiring_unchanged(&view.root, &files, input)
        });
        verified.then_some(view)
    }

    pub(super) fn private_call_module(&self, file: &Path) -> Option<VisibilityModule> {
        if !self.unrelocated(file) || !self.macro_free() {
            return None;
        }
        self.visibility_module(file)
    }

    fn visibility_module(&self, file: &Path) -> Option<VisibilityModule> {
        if let Some(found) = self
            .visibility_modules
            .lock()
            .expect("visibility cache mutex poisoned")
            .get(file)
        {
            return found.clone();
        }
        let result = (|| {
            let (_, src, modules) = source_module_path_inner(&self.root, file, true)?;
            // 全体の証明が標準名の置換も検査した場合は、局所の glob 一律禁止を重ねない。
            let verify_syntax = !self.allows_reexport_paths();
            let mut owner = None;
            for root in [src.join("lib.rs"), src.join("main.rs")] {
                // crate root 自体を別モジュールとして読む配置は所有元が一意でない。
                if modules.is_empty()
                    && private_module_file_from_roots(
                        &self.root,
                        &src,
                        &[file.file_stem()?.to_str()?.to_owned()],
                        verify_syntax,
                        true,
                        [root.clone()],
                    )
                    .as_deref()
                        == Some(file)
                {
                    return None;
                }
                if private_module_file_from_roots(
                    &self.root,
                    &src,
                    &modules,
                    verify_syntax,
                    true,
                    [root.clone()],
                )
                .as_deref()
                    == Some(file)
                {
                    if owner.is_some() {
                        // 同じファイルでも複数の crate に属すれば所有元は一意でない。
                        return None;
                    }
                    owner = Some(root);
                }
            }
            Some(VisibilityModule {
                crate_root: owner?,
                modules,
            })
        })();
        self.visibility_modules
            .lock()
            .expect("visibility cache mutex poisoned")
            .insert(file.to_owned(), result.clone());
        result
    }

    fn has_safe_private_import_environment(&self) -> bool {
        *self
            .safe_private_import_environment
            .get_or_init(|| scan_safe_private_import_environment(&self.root))
    }

    fn static_module_bindings(&self) -> Option<&HashSet<String>> {
        self.static_module_bindings
            .get_or_init(|| scan_static_module_placement(&self.root))
            .as_ref()
    }

    /// プロジェクトのどこかの `macro_rules!` が `#[macro_export]` などで `name` を crate root へ
    /// 公開し得るか (走査できなければ真)。
    pub(super) fn may_define_macro(&self, name: &str) -> bool {
        self.macro_definitions
            .get_or_init(|| scan_macro_definitions(&self.root))
            .as_ref()
            .is_none_or(|names| names.contains(name))
    }

    pub(super) fn staged_bindings(&self) -> Option<&Self> {
        self.staged_bindings_for(self.staged_binding_diff.as_deref().unwrap_or(""))
    }

    pub(super) fn staged_bindings_for(&self, input: &str) -> Option<&Self> {
        self.staged_bindings
            .get_or_init(|| {
                let snapshot = crate::commands::git_snapshot::IndexSnapshot::for_bindings(
                    self.root.to_str()?,
                )?;
                let root = Path::new(snapshot.dir().ok()?);
                let files = crate::engine::diff::parse_unified_diff(input);
                let mut project = Self::new(root, false, false, false, &files, input);
                project.callable_cargo_known = super::rust_cargo_inputs_unchanged(
                    &crate::engine::diff::scan_blocks(input),
                    &files,
                    true,
                );
                project.layout_checkout.clone_from(&self.layout_checkout);
                project.cargo_configs =
                    super::rust_cargo_config::CargoConfigProof::capture(&self.layout_checkout);
                Some(Box::new((snapshot, project)))
            })
            .as_deref()
            .map(|(_, project)| project)
    }

    pub(super) fn allows_macro_value_bindings(&self) -> bool {
        *self.safe_macro_value_environment.get_or_init(|| {
            let mut shadowable_attributes = HashSet::new();
            let mut other_bindings = HashSet::new();
            let safe = scan_safe_source_environment(&self.root, |source| {
                if source_defines_allowed_macro(source) || source_may_include_elsewhere(source) {
                    return true;
                }
                let Ok(tree) = parser::parse_source(source, LangId::Rust) else {
                    return true;
                };
                let root = tree.root_node();
                if root.has_error() {
                    return true;
                }
                let mut pending = vec![root];
                while let Some(node) = pending.pop() {
                    if node.is_missing() || node.kind() == "extern_crate_declaration" {
                        return true;
                    }
                    if matches!(node.kind(), "attribute_item" | "inner_attribute_item") {
                        let Some(names) = super::rust_std_origin::safe_attribute(node, source)
                        else {
                            return true;
                        };
                        shadowable_attributes.extend(names);
                    }
                    if node.kind() == "macro_definition" {
                        let Some(name) = node
                            .child_by_field_name("name")
                            .and_then(|name| name.utf8_text(source).ok())
                        else {
                            return true;
                        };
                        let name = name.strip_prefix("r#").unwrap_or(name);
                        if crate::engine::rust_scope::is_value_expression_macro(name) {
                            return true;
                        }
                        other_bindings.insert(name.to_owned());
                    }
                    if node.kind() == "use_declaration" {
                        let Ok(text) = node.utf8_text(source) else {
                            return true;
                        };
                        if text.contains('*') {
                            return true;
                        }
                        for token in text.split(|ch: char| !ch.is_alphanumeric() && ch != '_') {
                            if crate::engine::rust_scope::is_value_expression_macro(token) {
                                return true;
                            }
                            other_bindings.insert(token.to_owned());
                        }
                    }
                    let mut cursor = node.walk();
                    pending.extend(node.named_children(&mut cursor));
                }
                false
            });
            safe && shadowable_attributes.is_disjoint(&other_bindings)
        })
    }

    /// A leaf module has no other source file in its privacy subtree. A
    /// separate include! could inline it into a crate root, so reject that
    /// project-wide before using the local declaration as a leaf proof.
    pub(super) fn private_source_is_leaf(&self, source_path: &str) -> bool {
        if !self.private_scope_wiring_unchanged
            || !self.layout_cargo_settings_known()
            || !*self
                .no_source_inclusion
                .get_or_init(|| scan_no_source_inclusion(&self.root))
        {
            return false;
        }
        if let Some(found) = self
            .leaf_sources
            .lock()
            .expect("Rust leaf cache mutex poisoned")
            .get(source_path)
        {
            return *found;
        }
        let found = (|| {
            let file = self.root.join(source_path);
            if !file
                .canonicalize()
                .ok()?
                .starts_with(self.canonical_root.as_ref()?)
            {
                return None;
            }
            let source = read_input_file(&file)?;
            Some(!source_has_external_child_module(&source))
        })()
        .unwrap_or(false);
        self.leaf_sources
            .lock()
            .expect("Rust leaf cache mutex poisoned")
            .insert(source_path.to_owned(), found);
        found
    }
}

/// A worktree-only owner cannot prove the old binding when the diff changes
/// module declarations, imports, or attributes anywhere in Rust sources.
pub(super) fn module_wiring_unchanged(
    dir: &Path,
    diff_files: &[DiffFile],
    diff_input: &str,
) -> bool {
    wiring_unchanged(dir, diff_files, diff_input, module_wiring)
}

pub(super) fn private_scope_wiring_unchanged(
    dir: &Path,
    diff_files: &[DiffFile],
    diff_input: &str,
) -> bool {
    wiring_unchanged(dir, diff_files, diff_input, private_module_wiring)
}

fn wiring_unchanged(
    dir: &Path,
    diff_files: &[DiffFile],
    diff_input: &str,
    collect: fn(&[u8]) -> Option<Vec<Vec<u8>>>,
) -> bool {
    diff_files.iter().all(|file| {
        let path = &file.new_path;
        if !Path::new(path).extension().is_some_and(|ext| {
            ext.to_str()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("rs"))
        }) {
            return true;
        }
        let Some(new_source) = read_input_file(&dir.join(path)) else {
            return false;
        };
        let Some(old_source) =
            crate::engine::diff::reconstruct_old_source(diff_input, path, &new_source)
        else {
            return false;
        };
        match (collect(&old_source.source), collect(&new_source)) {
            (Some(old), Some(new)) => old == new,
            _ => false,
        }
    })
}

fn module_wiring(source: &[u8]) -> Option<Vec<Vec<u8>>> {
    collect_module_wiring(source, false)
}

fn private_module_wiring(source: &[u8]) -> Option<Vec<Vec<u8>>> {
    collect_module_wiring(source, true)
}

fn collect_module_wiring(source: &[u8], ignore_builtin_body_macros: bool) -> Option<Vec<Vec<u8>>> {
    let tree = parser::parse_source(source, LangId::Rust).ok()?;
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }
    let mut declarations = Vec::new();
    let mut pending = vec![Some(root)];
    while let Some(step) = pending.pop() {
        let Some(node) = step else {
            declarations.push(b"}".to_vec());
            continue;
        };
        let safe_body_macro = ignore_builtin_body_macros
            && node.kind() == "macro_invocation"
            && node
                .child_by_field_name("macro")
                .and_then(|name| name.utf8_text(source).ok())
                .is_some_and(is_allowed_expression_macro)
            && has_function_ancestor(node);
        if !safe_body_macro
            && matches!(
                node.kind(),
                "mod_item"
                    | "use_declaration"
                    | "extern_crate_declaration"
                    | "attribute_item"
                    | "inner_attribute_item"
                    | "macro_invocation"
                    | "macro_definition"
            )
        {
            if ignore_builtin_body_macros
                && node.kind() == "mod_item"
                && let Some(body) = node.child_by_field_name("body")
            {
                if source.get(body.start_byte()) != Some(&b'{')
                    || source.get(body.end_byte().checked_sub(1)?) != Some(&b'}')
                {
                    return None;
                }
                // inline module の通常の関数本文は配置ではない。閉じ位置は名前のスコープを保つ。
                declarations.push(
                    source
                        .get(node.start_byte()..body.start_byte() + 1)?
                        .to_vec(),
                );
                pending.push(None);
            } else {
                declarations.push(source.get(node.byte_range())?.to_vec());
            }
        }
        let mut cursor = node.walk();
        let mut children = node.named_children(&mut cursor).collect::<Vec<_>>();
        children.reverse();
        pending.extend(children.into_iter().map(Some));
    }
    Some(declarations)
}

fn has_function_ancestor(mut node: Node<'_>) -> bool {
    while let Some(parent) = node.parent() {
        if matches!(parent.kind(), "function_item" | "closure_expression") {
            return true;
        }
        node = parent;
    }
    false
}

impl RustPathOwnerIndex {
    /// Resolve both files in one ordinary crate before comparing the private
    /// declaration's module subtree. Unknown layouts never exclude a caller.
    pub(super) fn private_subtree_contains(
        &self,
        dir: &Path,
        source_path: &str,
        ref_path: &str,
        project: &RustPathOwnerProjectProof,
    ) -> Option<bool> {
        if !project.allows_private_scope_paths() {
            return None;
        }
        let source_file = Path::new(source_path);
        let ref_file = Path::new(ref_path);
        let (source_package, source_src, source_modules) = source_module_path(dir, source_file)?;
        let (ref_package, ref_src, ref_modules) = source_module_path(dir, ref_file)?;
        if source_package != ref_package || source_src != ref_src {
            return None;
        }
        if private_module_file(dir, &source_src, &source_modules, false)? != source_file
            || private_module_file(dir, &ref_src, &ref_modules, false)? != ref_file
        {
            return None;
        }
        Some(ref_modules.starts_with(&source_modules))
    }

    /// A plain root-level import can identify an unrelated declaration even
    /// when an unsafe construct in another file prevents a project-wide proof.
    /// 関数の本体のマクロが参照ファイルを変更元の block の中へ取り込んでも、crate 起点の
    /// import はその名前空間で外側の宣言を隠す。import の先には、参照と同じ名前空間の
    /// 無条件の宣言を求める (型だけの import は値の参照を外側へ素通しする)。取り込み先は
    /// 別の crate のこともあるので、import のパスの module 名を `use` で別名に束縛する宣言が
    /// プロジェクトのどこかにあれば証明しない (`use crate::store as source;` など)。
    #[allow(clippy::too_many_arguments)]
    pub(super) fn imports_other_private_owner(
        &self,
        dir: &Path,
        source_path: &str,
        ref_path: &str,
        ref_source: &[u8],
        name: &str,
        namespace: ImportNamespace,
        project: &RustPathOwnerProjectProof,
    ) -> bool {
        if blake3::hash(ref_source) != self.source_hash
            || !project.private_scope_wiring_unchanged
            || source_can_retarget_private_scope(ref_source)
        {
            return false;
        }
        let Some(module_bindings) = project.static_module_bindings() else {
            return false;
        };
        let Some(import) = self.direct_imports.get(name) else {
            return false;
        };
        let source_file = Path::new(source_path);
        let ref_file = Path::new(ref_path);
        let Some((source_package, source_src, source_modules)) =
            source_module_path(dir, source_file)
        else {
            return false;
        };
        let Some((ref_package, ref_src, ref_modules)) = source_module_path(dir, ref_file) else {
            return false;
        };
        if source_package != ref_package || source_src != ref_src || source_modules.is_empty() {
            return false;
        }
        let target_modules = &import[1..import.len() - 1];
        if ref_modules.starts_with(&source_modules)
            || target_modules.starts_with(&source_modules)
            || target_modules
                .iter()
                .any(|segment| module_bindings.contains(segment))
        {
            return false;
        }
        let Some(verified_ref) = private_module_file(dir, &ref_src, &ref_modules, true) else {
            return false;
        };
        let Some(target_file) = private_module_file(dir, &ref_src, target_modules, true) else {
            return false;
        };
        if verified_ref != ref_file || target_file == source_file {
            return false;
        }
        let Some(target_source) = read_input_file(&dir.join(&target_file)) else {
            return false;
        };
        if source_can_retarget_private_scope(&target_source) {
            return false;
        }
        let Ok(tree) = parser::parse_source(&target_source, LangId::Rust) else {
            return false;
        };
        let root = tree.root_node();
        if root.has_error() {
            return false;
        }
        let mut cursor = root.walk();
        root.named_children(&mut cursor).any(|child| {
            declares_in_namespace(child, &target_source, namespace)
                && child
                    .child_by_field_name("name")
                    .and_then(|node| node.utf8_text(&target_source).ok())
                    == Some(name)
                && unconditional_declaration(child, &target_source)
        })
    }

    /// 一致を確かめたソースの場所を忘れる (そのソースを手放した後で、同じ場所に別のソースが
    /// 置かれ得るため)。
    pub(super) fn forget_verified_source(&self) {
        self.verified_source.set(None);
    }

    /// `source` がこの索引を作ったソースか。一致を確かめたソースは場所と長さで覚える (覚えた
    /// ソースを手放したら [`Self::forget_verified_source`] を呼ぶこと)。
    fn same_source(&self, source: &[u8]) -> bool {
        let key = (source.as_ptr() as usize, source.len());
        if self.verified_source.get() == Some(key) {
            return true;
        }
        let same = blake3::hash(source) == self.source_hash;
        if same {
            self.verified_source.set(Some(key));
        }
        same
    }

    /// `check` の結果を (スコープ, 種類, 名前) ごとに覚えて返す。
    fn rebinds(
        &self,
        scope: Node<'_>,
        kind: Rebind,
        name: &str,
        check: impl FnOnce() -> bool,
    ) -> bool {
        let key = (scope.id(), kind, name.to_owned());
        if let Some(&found) = self.rebinds.borrow().get(&key) {
            return found;
        }
        let found = check();
        self.rebinds.borrow_mut().insert(key, found);
        found
    }

    /// A direct declaration in this file owns an unqualified reference unless
    /// a nested scope can rebind it. This does not require project-wide module
    /// evidence, so unrelated test modules do not disable the proof.
    pub(super) fn plain_local_owner(&self, node: Node<'_>, source: &[u8], name: &str) -> bool {
        if node.kind() != "type_identifier"
            || !self.local_type_names.contains(name)
            || !self.same_source(source)
            || has_uncertain_ancestor(node)
        {
            return false;
        }
        let mut ancestor = node.parent();
        while let Some(scope) = ancestor {
            if matches!(scope.kind(), "impl_item" | "trait_item") && has_attribute_prefix(scope) {
                return false;
            }
            if matches!(
                scope.kind(),
                "function_item" | "closure_expression" | "block"
            ) && (has_attribute_prefix(scope)
                || self.rebinds(scope, Rebind::Type, name, || {
                    scope_can_rebind_name(scope, source, name)
                }))
            {
                return false;
            }
            ancestor = scope.parent();
        }
        true
    }

    pub(super) fn closed_local_rule_calls(&self, mut node: Node<'_>, source: &[u8]) -> bool {
        let position = node.start_byte();
        *self.closed_local_rule.get_or_init(|| {
            while let Some(parent) = node.parent() {
                node = parent;
            }
            super::rust_macro_site::closed_local_rule_call(node, source)
        }) == Some(position)
    }

    /// 直下の一意な関数・定数・static の束縛を、利用位置の再束縛と区別する。
    pub(super) fn plain_local_value_owner(
        &self,
        node: Node<'_>,
        source: &[u8],
        name: &str,
    ) -> bool {
        self.local_value_owner_beside(node, source, name, None)
    }

    /// `proven_macro` は標準の書式マクロと証明した呼び出しで、文の位置にあっても束縛を
    /// 作らないので再束縛の障壁にしない。ほかのマクロは従来どおり障壁にする。
    pub(super) fn local_value_owner_beside(
        &self,
        node: Node<'_>,
        source: &[u8],
        name: &str,
        proven_macro: Option<usize>,
    ) -> bool {
        if node.kind() != "identifier"
            || !self.local_value_names.contains(name)
            || !self.same_source(source)
            || has_uncertain_ancestor(node)
        {
            return false;
        }
        let mut ancestor = node.parent();
        while let Some(scope) = ancestor {
            if matches!(
                scope.kind(),
                "function_item"
                    | "closure_expression"
                    | "block"
                    | "match_arm"
                    | "impl_item"
                    | "trait_item"
            ) && (has_attribute_prefix(scope)
                || self.rebinds(scope, Rebind::Value(proven_macro), name, || {
                    scope_can_rebind_import_name_beside(scope, source, name, proven_macro)
                }))
            {
                return false;
            }
            ancestor = scope.parent();
        }
        true
    }

    pub(super) fn is_tracked_direct_import_site(
        &self,
        mut node: Node<'_>,
        source: &[u8],
        name: &str,
    ) -> bool {
        let Some(target) = self.direct_imports.get(name) else {
            return false;
        };
        loop {
            if node.kind() == "use_declaration" {
                return node
                    .utf8_text(source)
                    .ok()
                    .and_then(plain_crate_import_path)
                    .as_ref()
                    == Some(target);
            }
            let Some(parent) = node.parent() else {
                return false;
            };
            node = parent;
        }
    }

    pub(super) fn plain_import_binding_safe(
        &self,
        node: Node<'_>,
        source: &[u8],
        name: &str,
    ) -> bool {
        self.import_binding_safe_beside(node, source, name, None)
    }

    /// `proven_macro` の扱いは [`Self::local_value_owner_beside`] と同じ。
    pub(super) fn import_binding_safe_beside(
        &self,
        node: Node<'_>,
        source: &[u8],
        name: &str,
        proven_macro: Option<usize>,
    ) -> bool {
        if !self.direct_imports.contains_key(name)
            || !self.same_source(source)
            || has_uncertain_ancestor(node)
        {
            return false;
        }
        let mut ancestor = node.parent();
        while let Some(scope) = ancestor {
            if matches!(
                scope.kind(),
                "function_item" | "closure_expression" | "block" | "match_arm"
            ) && (has_attribute_prefix(scope)
                || self.rebinds(scope, Rebind::Value(proven_macro), name, || {
                    scope_can_rebind_import_name_beside(scope, source, name, proven_macro)
                }))
            {
                return false;
            }
            ancestor = scope.parent();
        }
        true
    }

    pub(super) fn build(mut node: Node<'_>, source: &[u8]) -> Self {
        while let Some(parent) = node.parent() {
            node = parent;
        }
        let mut direct_imports = HashMap::new();
        let mut ambiguous_imports = HashSet::new();
        let mut type_counts = HashMap::<String, usize>::new();
        let mut function_counts = HashMap::<String, usize>::new();
        let mut direct_constants = HashSet::new();
        let mut root_imports = Vec::new();
        let mut import_names = Some(HashSet::new());
        let mut glob_import = false;
        let mut root_uncertain = node.has_error();
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "inner_attribute_item"
                && super::rust_std_origin::safe_attribute(child, source).is_none()
            {
                root_uncertain = true;
            }
            if matches!(
                child.kind(),
                "macro_invocation" | "extern_crate_declaration"
            ) {
                root_uncertain = true;
            }
            if child.kind() != "use_declaration" {
                if !has_attribute_prefix(child)
                    && let Some(name) = child
                        .child_by_field_name("name")
                        .and_then(|name| name.utf8_text(source).ok())
                {
                    if matches!(
                        child.kind(),
                        "struct_item" | "enum_item" | "union_item" | "trait_item"
                    ) {
                        *type_counts.entry(name.to_owned()).or_default() += 1;
                    } else if matches!(child.kind(), "function_item" | "const_item" | "static_item")
                    {
                        let name = crate::language::normalize_identifier(LangId::Rust, name);
                        if child.kind() == "const_item" {
                            direct_constants.insert(name.clone());
                        }
                        *function_counts.entry(name.into_owned()).or_default() += 1;
                    }
                }
                continue;
            }
            if let Ok(text) = child.utf8_text(source) {
                root_imports.push(text);
            } else {
                root_uncertain = true;
            }
            let bound = root_use_names(child, source).or_else(|| {
                glob_import = true;
                use_names(child, source, true)
            });
            import_names = import_names.zip(bound).map(|(mut names, found)| {
                names.extend(found);
                names
            });
            let Some(path) = child
                .utf8_text(source)
                .ok()
                .and_then(plain_crate_import_path)
            else {
                continue;
            };
            let name = path.last().cloned().unwrap_or_default();
            if has_attribute_prefix(child) {
                ambiguous_imports.insert(name);
                continue;
            }
            if direct_imports.insert(name.clone(), path).is_some() {
                ambiguous_imports.insert(name);
            }
        }
        for name in ambiguous_imports {
            direct_imports.remove(&name);
        }
        direct_imports.retain(|name, _| {
            root_imports
                .iter()
                .filter(|import| import.contains('*') || import.contains(name))
                .count()
                == 1
        });
        let safe_name = |name: &str| {
            !root_uncertain
                && import_names
                    .as_ref()
                    .is_some_and(|names| !names.contains(name))
        };
        let local_type_names = type_counts
            .into_iter()
            .filter_map(|(name, count)| {
                (count == 1 && !glob_import && safe_name(&name)).then_some(name)
            })
            .collect();
        // ファイル直下の無条件の const は同じモジュールの glob より優先される。
        let local_value_names = function_counts
            .into_iter()
            .filter_map(|(name, count)| {
                (count == 1
                    && safe_name(&name)
                    && (!glob_import || direct_constants.contains(name.as_str())))
                .then_some(name)
            })
            .collect();
        Self {
            current: FileEvidence::collect(node, source).map(Arc::new),
            root_associated_paths: root_associated_paths(node, source),
            unshadowed_qualified_paths: root_associated_paths_with_attributes(
                node, source, true, false,
            ),
            associated_types: RefCell::new(HashMap::new()),
            external_associated_types: RefCell::new(HashMap::new()),
            external_associated_paths: OnceLock::new(),
            registry_qualifiers: RefCell::new(HashMap::new()),
            source_hash: blake3::hash(source),
            direct_imports,
            qualified_call_owners: RefCell::new(HashMap::new()),
            field_types: std::cell::OnceCell::new(),
            independent_pointers: Default::default(),
            macro_paths: std::cell::OnceCell::new(),
            json_calls: RefCell::default(),
            tracing_fields: Default::default(),
            visibility_imports: Default::default(),
            local_type_names,
            local_value_names,
            closed_local_rule: OnceLock::new(),
            module_context: OnceLock::new(),
            resolved_paths: RefCell::new(HashMap::new()),
            enum_files: RefCell::new(HashMap::new()),
            enum_owners: RefCell::new(HashMap::new()),
            nominal_sites: Default::default(),
            local_inherent_sites: Default::default(),
            format_environment: OnceLock::new(),
            rebinds: RefCell::new(HashMap::new()),
            verified_source: std::cell::Cell::new(None),
        }
    }

    /// Returns a repository-relative file only when the type is locally
    /// declared in the module selected by this reference.
    pub(super) fn type_owner(
        &self,
        node: Node<'_>,
        source: &[u8],
        dir: &Path,
        ref_path: &str,
        project: &RustPathOwnerProjectProof,
    ) -> Option<PathBuf> {
        let current = self.current.as_ref()?;
        if node.kind() != "type_identifier" || has_uncertain_ancestor(node) {
            return None;
        }
        let name = node.utf8_text(source).ok()?;
        let ref_file = Path::new(ref_path);
        let parent = node.parent()?;
        if matches!(
            parent.kind(),
            "scoped_type_identifier" | "scoped_identifier"
        ) {
            if !project.allows_module_paths() {
                return None;
            }
            if !parent
                .child_by_field_name("name")
                .is_some_and(|last| last.id() == node.id())
            {
                return None;
            }
            let segments = path_segments(parent, source)?;
            let owner = self.resolve_qualified(dir, ref_file, &segments, project)?;
            let evidence = self.evidence(ref_file, &owner, project)?;
            return evidence.types.contains(name).then_some(owner);
        }
        current.types.contains(name).then(|| ref_file.to_owned())
    }

    fn field_types(&self, mut node: Node<'_>, source: &[u8]) -> &HashSet<String> {
        self.field_types.get_or_init(|| {
            while let Some(parent) = node.parent() {
                node = parent;
            }
            let mut cursor = node.walk();
            // 型パラメータのない無条件の構造体のフィールドで、型として使う単独名。
            // 同じモジュールのこの名前は、モジュールへの別名にはなれない。
            let mut field_types = HashSet::new();
            for item in node.named_children(&mut cursor).filter(|item| {
                item.kind() == "struct_item"
                    && item.child_by_field_name("type_parameters").is_none()
                    && !has_attribute_prefix(*item)
            }) {
                let Some(fields) = item.child_by_field_name("body") else {
                    continue;
                };
                for field in fields.named_children(&mut fields.walk()) {
                    if has_attribute_prefix(field) {
                        continue;
                    }
                    let mut pending: Vec<_> =
                        field.child_by_field_name("type").into_iter().collect();
                    while let Some(ty) = pending.pop() {
                        match ty.kind() {
                            "type_identifier" => {
                                if let Ok(name) = ty.utf8_text(source) {
                                    field_types.insert(name.to_owned());
                                }
                            }
                            "generic_type" => {
                                if let Some(args) = ty.child_by_field_name("type_arguments") {
                                    pending.extend(args.named_children(&mut args.walk()));
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            field_types
        })
    }

    pub(super) fn qualified_call_owner(
        &self,
        node: Node<'_>,
        source: &[u8],
        file: &Path,
        project: &RustPathOwnerProjectProof,
    ) -> Option<QualifiedCallOwner> {
        let parent = node.parent()?;
        if parent.kind() != "scoped_identifier"
            || parent.child_by_field_name("name") != Some(node)
            || has_uncertain_ancestor(node)
        {
            return None;
        }
        let path = path_segments(parent, source)?;
        let first = path.first()?.as_str();
        if first == "Self" {
            return Some(QualifiedCallOwner::Type);
        }
        if !matches!(first, "crate" | "self" | "super")
            && !self.root_associated_paths.contains(&parent.id())
        {
            return None;
        }
        if path.len() == 2 && self.field_types(node, source).contains(first) {
            return Some(QualifiedCallOwner::Type);
        }
        if let Some(owner) = self.qualified_call_owners.borrow().get(&path) {
            return owner.clone();
        }
        let owner = project
            .callable_layout_stable()
            .then(|| super::rust_reexport_owner::declared_call_owner(project, file, &path))
            .flatten()
            .filter(|_| project.unrelocated(file));
        self.qualified_call_owners
            .borrow_mut()
            .insert(path, owner.clone());
        owner
    }

    pub(super) fn free_function_owner(
        &self,
        node: Node<'_>,
        source: &[u8],
        dir: &Path,
        ref_path: &str,
        project: &RustPathOwnerProjectProof,
    ) -> Option<PathBuf> {
        let current = self.current.as_ref()?;
        if node.kind() != "identifier" || has_uncertain_ancestor(node) {
            return None;
        }
        let name = node.utf8_text(source).ok()?;
        let ref_file = Path::new(ref_path);
        let parent = node.parent()?;
        if parent.kind() == "scoped_identifier" {
            if !parent
                .child_by_field_name("name")
                .is_some_and(|last| last.id() == node.id())
                || !project.allows_module_paths()
            {
                return None;
            }
            let segments = path_segments(parent, source)?;
            let owner = self.resolve_qualified(dir, ref_file, &segments, project)?;
            let evidence = self.evidence(ref_file, &owner, project)?;
            return evidence.free_functions.contains(name).then_some(owner);
        }
        current
            .free_functions
            .contains(name)
            .then(|| ref_file.to_owned())
    }

    /// パスの途中の名前は、自由関数を指さない。
    pub(super) fn is_nonterminal_path(node: Node<'_>) -> bool {
        if has_uncertain_ancestor(node) {
            return false;
        }
        let mut child = node;
        while let Some(parent) = child.parent() {
            if !matches!(
                parent.kind(),
                "scoped_identifier" | "scoped_type_identifier"
            ) {
                return false;
            }
            if parent
                .child_by_field_name("path")
                .is_some_and(|path| path.id() == child.id())
            {
                return true;
            }
            child = parent;
        }
        false
    }

    /// 宣言または named import から型と確認できるパスは、別 module の自由関数を指さない。
    pub(super) fn is_local_associated_path(
        &self,
        node: Node<'_>,
        source: &[u8],
        dir: &Path,
        ref_path: &str,
        project: &RustPathOwnerProjectProof,
        cargo: &super::rust_cargo_reachability::RustCargoReachability,
    ) -> bool {
        if node.kind() != "identifier" || has_uncertain_ancestor(node) {
            return false;
        }
        let Some(parent) = node.parent() else {
            return false;
        };
        if parent.kind() != "scoped_identifier"
            || !parent
                .child_by_field_name("name")
                .is_some_and(|last| last.id() == node.id())
        {
            return false;
        }
        let Some(path) = path_segments(parent, source) else {
            return false;
        };
        if path.len() != 2 {
            return false;
        }
        if path[0] == "Self" {
            let mut ancestor = node.parent();
            while let Some(current) = ancestor {
                if current.kind() == "impl_item" {
                    return self.current.is_some();
                }
                ancestor = current.parent();
            }
            return false;
        }
        let ordinary_scope = self.root_associated_paths.contains(&parent.id());
        if ordinary_scope
            && self
                .current
                .as_ref()
                .is_some_and(|evidence| evidence.types.contains(&path[0]))
        {
            return true;
        }
        if ordinary_scope {
            let cached = self.associated_types.borrow().get(&path[0]).copied();
            let proven = cached.unwrap_or_else(|| {
                let proven = super::rust_reexport_owner::associated_type_binding(
                    &path[0], dir, ref_path, project,
                );
                self.associated_types
                    .borrow_mut()
                    .insert(path[0].clone(), proven);
                proven
            });
            if proven {
                return true;
            }
        }
        let cached = self
            .external_associated_types
            .borrow()
            .get(&path[0])
            .copied();
        let external = cached.unwrap_or_else(|| {
            let proven = super::rust_reexport_owner::external_associated_type_binding(
                &path[0], dir, ref_path, project, cargo,
            );
            self.external_associated_types
                .borrow_mut()
                .insert(path[0].clone(), proven);
            proven
        });
        external
            && (ordinary_scope
                || self
                    .external_associated_paths
                    .get_or_init(|| {
                        let mut root = node;
                        while let Some(parent) = root.parent() {
                            root = parent;
                        }
                        // path 依存の証明が、全ソースの標準マクロの束縛も確認している。
                        root_associated_paths_with_attributes(root, source, true, true)
                    })
                    .contains(&parent.id()))
    }

    pub(super) fn is_registry_qualified_call(
        &self,
        node: Node<'_>,
        source: &[u8],
        dir: &Path,
        ref_path: &str,
        project: &RustPathOwnerProjectProof,
    ) -> bool {
        let Some(path) = node.parent().filter(|path| {
            self.unshadowed_qualified_paths.contains(&path.id())
                && path
                    .child_by_field_name("name")
                    .is_some_and(|name| name.id() == node.id())
        }) else {
            return false;
        };
        if !path.parent().is_some_and(|call| {
            call.kind() == "call_expression"
                && call
                    .child_by_field_name("function")
                    .is_some_and(|callee| callee.id() == path.id())
        }) {
            return false;
        }
        let Some(qualifier) = path
            .child_by_field_name("path")
            .and_then(|name| name.utf8_text(source).ok())
        else {
            return false;
        };
        if let Some(proven) = self.registry_qualifiers.borrow().get(qualifier) {
            return *proven;
        }
        let proven = super::rust_reexport_owner::registry_qualifier_binding(
            qualifier, dir, ref_path, project,
        );
        self.registry_qualifiers
            .borrow_mut()
            .insert(qualifier.to_owned(), proven);
        proven
    }

    fn resolve_qualified(
        &self,
        dir: &Path,
        ref_file: &Path,
        segments: &[String],
        project: &RustPathOwnerProjectProof,
    ) -> Option<PathBuf> {
        if let Some(cached) = self.resolved_paths.borrow().get(segments) {
            return cached.clone();
        }
        let resolved = (|| {
            let (package, src, current_modules) = self
                .module_context
                .get_or_init(|| source_module_path(dir, ref_file))
                .as_ref()?;
            let current_file = self.module_file(dir, ref_file, src, current_modules, project)?;
            if current_file != ref_file {
                return None;
            }
            let (first, rest) = segments.split_first()?;
            let mut target = match first.as_str() {
                "crate" => Vec::new(),
                "self" => current_modules.clone(),
                "super" => {
                    let mut parent = current_modules.clone();
                    parent.pop()?;
                    parent
                }
                _ => return None,
            };
            for segment in rest.iter().take(rest.len().saturating_sub(1)) {
                if segment == "super" && first == "super" {
                    target.pop()?;
                } else if segment == "self" {
                    continue;
                } else {
                    target.push(segment.clone());
                }
            }
            let owner = self.module_file(dir, ref_file, src, &target, project)?;
            owner.strip_prefix(package).ok()?;
            Some(owner)
        })();
        self.resolved_paths
            .borrow_mut()
            .insert(segments.to_vec(), resolved.clone());
        resolved
    }

    fn module_file(
        &self,
        dir: &Path,
        current: &Path,
        src: &Path,
        modules: &[String],
        project: &RustPathOwnerProjectProof,
    ) -> Option<PathBuf> {
        let lib = src.join("lib.rs");
        let main = src.join("main.rs");
        let root = match (checked_is_file(dir, &lib)?, checked_is_file(dir, &main)?) {
            (true, false) => lib,
            (false, true) => main,
            _ => return None,
        };
        let mut parent = root;
        let mut prefix = PathBuf::new();
        for module in modules {
            let evidence = self.evidence(current, &parent, project)?;
            if !evidence.modules.contains(module) {
                return None;
            }
            let base = src.join(&prefix);
            let file = base.join(format!("{module}.rs"));
            let nested = base.join(module).join("mod.rs");
            parent = match (checked_is_file(dir, &file)?, checked_is_file(dir, &nested)?) {
                (true, false) => file,
                (false, true) => nested,
                _ => return None,
            };
            prefix.push(module);
        }
        Some(parent)
    }

    fn evidence(
        &self,
        current: &Path,
        file: &Path,
        project: &RustPathOwnerProjectProof,
    ) -> Option<Arc<FileEvidence>> {
        if file == current {
            return self.current.clone();
        }
        if let Some(found) = project
            .files
            .lock()
            .expect("Rust owner file cache mutex poisoned")
            .get(file)
        {
            return found.clone();
        }
        let parsed = (|| {
            let full = project.root.join(file);
            if !full
                .canonicalize()
                .ok()?
                .starts_with(project.canonical_root.as_ref()?)
            {
                return None;
            }
            let source = read_input_file(&full)?;
            let tree = parser::parse_source(&source, LangId::Rust).ok()?;
            FileEvidence::collect(tree.root_node(), &source).map(Arc::new)
        })();
        project
            .files
            .lock()
            .expect("Rust owner file cache mutex poisoned")
            .insert(file.to_owned(), parsed.clone());
        parsed
    }
}

fn root_use_names(declaration: Node<'_>, source: &[u8]) -> Option<Vec<String>> {
    use_names(declaration, source, false)
}

/// `use` が束縛する名前 (別名を含む)。`skip_globs` なら名前を束縛しない glob を数えずに
/// 続け、そうでなければ glob を解釈できない形として `None` を返す。
fn use_names(declaration: Node<'_>, source: &[u8], skip_globs: bool) -> Option<Vec<String>> {
    let mut pending = vec![declaration.child_by_field_name("argument")?];
    let mut names = Vec::new();
    while let Some(node) = pending.pop() {
        match node.kind() {
            "use_wildcard" if skip_globs => {}
            "use_list" => {
                let mut cursor = node.walk();
                pending.extend(
                    node.named_children(&mut cursor)
                        .filter(|node| !matches!(node.kind(), "line_comment" | "block_comment")),
                );
            }
            "scoped_use_list" => {
                let mut cursor = node.walk();
                pending.push(
                    node.named_children(&mut cursor)
                        .find(|node| node.kind() == "use_list")?,
                );
            }
            "use_as_clause" => {
                let alias = node.child_by_field_name("alias")?.utf8_text(source).ok()?;
                names.push(alias.strip_prefix("r#").unwrap_or(alias).to_owned());
            }
            "scoped_identifier" => pending.push(node.child_by_field_name("name")?),
            "self" => {
                let list = node.parent()?;
                let group = list.parent()?;
                if list.kind() != "use_list" || group.kind() != "scoped_use_list" {
                    return None;
                }
                let path = group.child_by_field_name("path")?;
                pending.push(if path.kind() == "scoped_identifier" {
                    path.child_by_field_name("name")?
                } else {
                    path
                });
            }
            "identifier" => {
                let name = node.utf8_text(source).ok()?;
                names.push(name.strip_prefix("r#").unwrap_or(name).to_owned());
            }
            _ => return None,
        }
    }
    Some(names)
}

#[derive(Default)]
struct AssociatedScope {
    parent: Option<usize>,
    names: HashSet<String>,
    opaque: bool,
}

/// 型名を再束縛し得るスコープだけを一度走査し、参照ごとの祖先の再走査を避ける。
fn root_associated_paths(root: Node<'_>, source: &[u8]) -> HashSet<usize> {
    root_associated_paths_with_attributes(root, source, false, false)
}

fn root_associated_paths_with_attributes(
    root: Node<'_>,
    source: &[u8],
    check_attributes: bool,
    standard_macros_proven: bool,
) -> HashSet<usize> {
    if root.has_error() {
        return HashSet::new();
    }
    let mut scopes = vec![AssociatedScope::default()];
    let mut paths = Vec::new();
    let mut pending = vec![(root, 0, "")];
    while let Some((node, enclosing, parent_kind)) = pending.pop() {
        if node.kind() == "mod_item" {
            if enclosing != 0
                && let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|name| name.utf8_text(source).ok())
            {
                if !name.is_ascii() {
                    scopes[enclosing].opaque = true;
                }
                scopes[enclosing]
                    .names
                    .insert(name.strip_prefix("r#").unwrap_or(name).to_owned());
            }
            continue;
        }
        let scope = if matches!(
            node.kind(),
            "block"
                | "function_item"
                | "impl_item"
                | "closure_expression"
                | "trait_item"
                | "struct_item"
                | "enum_item"
                | "union_item"
                | "type_item"
        ) {
            let scope = scopes.len();
            scopes.push(AssociatedScope {
                parent: Some(enclosing),
                opaque: check_attributes && transforming_scope_attribute(node, source),
                ..AssociatedScope::default()
            });
            scope
        } else {
            enclosing
        };
        match node.kind() {
            "macro_definition" => scopes[0].opaque = true,
            "macro_invocation"
                if matches!(
                    parent_kind,
                    "source_file" | "block" | "expression_statement"
                ) && !(standard_macros_proven
                    && node
                        .child_by_field_name("macro")
                        .filter(|callee| callee.kind() == "identifier")
                        .and_then(|callee| callee.utf8_text(source).ok())
                        .is_some_and(|name| {
                            is_allowed_expression_macro(name.strip_prefix("r#").unwrap_or(name))
                        })) =>
            {
                scopes[scope].opaque = true
            }
            "use_declaration" if scope != 0 => match root_use_names(node, source) {
                Some(names) => scopes[scope].names.extend(names),
                None => scopes[scope].opaque = true,
            },
            "extern_crate_declaration" if scope != 0 => scopes[scope].opaque = true,
            "type_parameter" | "struct_item" | "enum_item" | "union_item" | "type_item"
            | "trait_item"
                if scope != 0 =>
            {
                if let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|name| name.utf8_text(source).ok())
                {
                    let binding_scope = if node.kind() == "type_parameter" {
                        scope
                    } else {
                        enclosing
                    };
                    if binding_scope != 0 {
                        if !name.is_ascii() {
                            scopes[binding_scope].opaque = true;
                        }
                        scopes[binding_scope]
                            .names
                            .insert(name.strip_prefix("r#").unwrap_or(name).to_owned());
                    }
                }
            }
            "scoped_identifier" => {
                if let Some(path) = node.child_by_field_name("path")
                    && path.kind() == "identifier"
                    && let Ok(name) = path.utf8_text(source)
                {
                    paths.push((node.id(), scope, name.strip_prefix("r#").unwrap_or(name)));
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        pending.extend(
            node.named_children(&mut cursor)
                .map(|child| (child, scope, node.kind())),
        );
    }
    let mut proven = HashMap::new();
    paths
        .into_iter()
        .filter_map(|(path, mut scope, name)| {
            let mut visited = Vec::new();
            let safe = loop {
                if let Some(safe) = proven.get(&(scope, name)) {
                    break *safe;
                }
                visited.push(scope);
                if scopes[scope].opaque || scopes[scope].names.contains(name) {
                    break false;
                }
                let Some(parent) = scopes[scope].parent else {
                    break true;
                };
                scope = parent;
            };
            for scope in visited {
                proven.insert((scope, name), safe);
            }
            safe.then_some(path)
        })
        .collect()
}

pub(crate) fn scan_ordinary_sources(root: &Path) -> bool {
    scan_sources(root, source_can_retarget_modules)
}

fn scan_private_scope_sources(root: &Path) -> bool {
    scan_sources(root, source_can_retarget_private_scope)
}

fn scan_reexport_sources(root: &Path) -> bool {
    let mut shadowable_names = HashSet::new();
    let mut defined_macros = HashSet::new();
    let mut imports = Vec::new();
    let mut external_glob = false;
    let ordinary = scan_sources(root, |source| {
        let Ok(tree) = parser::parse_source(source, LangId::Rust) else {
            return true;
        };
        let root = tree.root_node();
        if root.has_error() {
            return true;
        }
        let mut pending = vec![root];
        while let Some(node) = pending.pop() {
            match node.kind() {
                "macro_invocation" => {
                    if !has_function_ancestor(node) {
                        return true;
                    }
                    let name = node
                        .child_by_field_name("macro")
                        .and_then(|name| name.utf8_text(source).ok());
                    if !name.is_some_and(is_allowed_expression_macro) {
                        return true;
                    }
                    shadowable_names.insert(name.unwrap().to_owned());
                }
                "macro_definition" => {
                    let Some(name) = node
                        .child_by_field_name("name")
                        .and_then(|name| name.utf8_text(source).ok())
                    else {
                        return true;
                    };
                    defined_macros.insert(name.to_owned());
                }
                "extern_crate_declaration" => return true,
                "attribute_item" | "inner_attribute_item" => {
                    let Some(names) = super::rust_std_origin::safe_attribute(node, source) else {
                        return true;
                    };
                    shadowable_names.extend(names);
                }
                "use_declaration" => {
                    let Ok(text) = node.utf8_text(source) else {
                        return true;
                    };
                    let argument = node.child_by_field_name("argument");
                    let anchored = argument
                        .and_then(|value| value.utf8_text(source).ok())
                        .is_some_and(|text| {
                            ["crate::", "self::", "super::"]
                                .iter()
                                .any(|prefix| text.trim_start().starts_with(prefix))
                        });
                    if text.contains('*') && !anchored {
                        external_glob = true;
                    }
                    imports.push(text.to_owned());
                }
                _ => {}
            }
            let mut cursor = node.walk();
            pending.extend(node.named_children(&mut cursor));
        }
        false
    });
    ordinary
        && shadowable_names.iter().all(|name| {
            !defined_macros.contains(name)
                && !external_glob
                && !imports.iter().any(|import| {
                    import
                        .split(|ch: char| !ch.is_alphanumeric() && ch != '_')
                        .any(|token| token == name)
                })
        })
}

/// `dir` の下の `path` が通常のファイル (symlink の先を含む) か。staged の snapshot が書き出さなかった
/// 要素のために、実際の checkout と有無が違いうるなら None (Issue #376、[`crate::git_support::TreeView::hides`])。
fn checked_is_file(dir: &Path, path: &Path) -> Option<bool> {
    (!crate::git_support::tree_view(dir).hides(path)).then(|| dir.join(path).is_file())
}

/// Cargo の manifest の依存の表 (古い綴りの `_` を含む)。
/// [`RustPathOwnerProjectProof::package_reaches`] の、path でたどれる package の実体。
#[derive(Default)]
struct DependencyClosure {
    packages: HashSet<crate::git_support::EntryId>,
}

/// root の直下の `target` の、Cargo が作るディレクトリ (中に Cargo の設定を置かない)。
const CARGO_OUTPUTS: [&str; 6] = [
    "doc",
    "tmp",
    "package",
    "nextest",
    "llvm-cov-target",
    "criterion",
];

/// manifest の依存の表 (`[dependencies]` などと `[target.<cfg>.dependencies]` など)。
fn dependency_tables(manifest: &toml::Value) -> Option<Vec<&toml::Value>> {
    let mut tables = Vec::new();
    for kind in DEPENDENCY_TABLES {
        tables.extend(manifest.get(kind));
    }
    if let Some(targets) = manifest.get("target") {
        for platform in targets.as_table()?.values() {
            for kind in DEPENDENCY_TABLES {
                tables.extend(platform.as_table()?.get(kind));
            }
        }
    }
    Some(tables)
}

/// manifest (`directory` は root 相対の置き場所) の path の依存 (依存の表・`[workspace.dependencies]`・
/// `[patch.*]`・`[replace]`) の行き先 (root 相対。root の外は `None`)。`root` は絶対パスで、行き先は
/// root の外を回る (`../../ws/target/probe`) パスや絶対パスも字面で畳む。`base` の付いた path は解け
/// ないので、読めない形と同じく全体を `None` にする。
pub(super) fn manifest_path_dependencies(
    root: &Path,
    manifest: &toml::Value,
    directory: &Path,
) -> Option<Vec<Option<PathBuf>>> {
    Some(
        manifest_path_dependency_targets(root, manifest, directory)?
            .into_iter()
            .map(|target| target.and_then(|target| relative_to_root(root, &target)))
            .collect(),
    )
}

/// manifest の path の依存 ([`manifest_path_dependencies`]) のどれかが、root の外を指すか。実体の
/// パス (在れば symlink・`\\?\` を解いたもの) が root (実体) で始まるかを、大文字小文字を区別して
/// 比べる (大文字小文字を区別するファイルシステムの `../../WS/probe` は root の `ws` と別)。行き先を
/// 決められなければ真。
pub(super) fn path_dependencies_leave_root(
    root: &Path,
    manifest: &toml::Value,
    directory: &Path,
) -> Option<bool> {
    Some(
        manifest_path_dependency_targets(root, manifest, directory)?
            .iter()
            .any(|target| {
                target
                    .as_ref()
                    .is_none_or(|target| !target.starts_with(root))
            }),
    )
}

/// `directory` が Git のディレクトリの形か: `HEAD` が `ref: refs/` か 16 進のコミット (40 か 64 桁) で
/// 始まり、`config` のファイルと `refs`・`objects` のディレクトリがある (`HEAD` という名前のファイルが
/// あるだけの普通のディレクトリの下を読み飛ばさない)。
fn git_directory_shape(directory: &Path) -> bool {
    let head = directory.join("HEAD");
    let Ok(metadata) = std::fs::symlink_metadata(&head) else {
        return false;
    };
    if !metadata.is_file() || metadata.len() > 4096 {
        return false;
    }
    let Ok(text) = std::fs::read_to_string(&head) else {
        return false;
    };
    let text = text.trim();
    let commit = |text: &str| {
        matches!(text.len(), 40 | 64) && text.bytes().all(|byte| byte.is_ascii_hexdigit())
    };
    (text.starts_with("ref: refs/") || commit(text))
        && directory.join("config").is_file()
        && directory.join("refs").is_dir()
        && directory.join("objects").is_dir()
}

/// package (`directory` の manifest) が `package.workspace` で、走査が読まない workspace (root の外か、
/// root の直下の `target`・`.git` の下) を名指すか (継承する依存 `probe.workspace = true` の先を、
/// その workspace の manifest から読む)。古い `[project]` の表 (`[package]` と同じく読まれる。依存の
/// たどり方 (`in_root_workspace`) は `[project]` の `workspace` を読まない) と、形が違うものも真。
pub(super) fn package_workspace_outside_root(
    root: &Path,
    manifest: &toml::Value,
    directory: &Path,
) -> bool {
    if manifest.get("project").is_some() {
        return true;
    }
    let Some(workspace) = manifest
        .get("package")
        .and_then(|package| package.get("workspace"))
    else {
        return false;
    };
    let Some(workspace) = workspace.as_str() else {
        return true;
    };
    absolute_lexical(&root.join(directory), Path::new(workspace)).is_none_or(|joined| {
        let resolved = joined.canonicalize().unwrap_or(joined);
        resolved
            .strip_prefix(root)
            .ok()
            .is_none_or(under_root_git_or_target)
    })
}

/// manifest の path の依存の行き先 (root を絶対パスにして字面で畳み、在れば実体にしたもの)。
/// 字面で畳めなければ `None` の要素。
fn manifest_path_dependency_targets(
    root: &Path,
    manifest: &toml::Value,
    directory: &Path,
) -> Option<Vec<Option<PathBuf>>> {
    let mut specs = Vec::new();
    for table in dependency_tables(manifest)?.into_iter().chain(
        manifest
            .get("workspace")
            .and_then(|workspace| workspace.get("dependencies")),
    ) {
        specs.extend(table.as_table()?.values());
    }
    if let Some(patches) = manifest.get("patch") {
        for registry in patches.as_table()?.values() {
            specs.extend(registry.as_table()?.values());
        }
    }
    if let Some(replace) = manifest.get("replace") {
        specs.extend(replace.as_table()?.values());
    }
    let mut out = Vec::new();
    for spec in specs {
        let toml::Value::Table(fields) = spec else {
            continue;
        };
        if fields.get("base").is_some() {
            return None;
        }
        let Some(path) = fields.get("path") else {
            continue;
        };
        // Cargo と同じく字面で畳んだ行き先を、在れば実体 (symlink・`\\?\`・`/private/var` の別名を
        // 解いたもの) にする。
        let joined = absolute_lexical(&root.join(directory), Path::new(path.as_str()?));
        out.push(joined.map(|joined| joined.canonicalize().unwrap_or(joined)));
    }
    Some(out)
}

/// manifest の path の依存 ([`manifest_path_dependencies`]) が root の直下の `target`・`.git` を指すか。
pub(super) fn path_dependencies_reach_unscanned(
    root: &Path,
    manifest: &toml::Value,
    directory: &Path,
) -> Option<bool> {
    Some(
        manifest_path_dependencies(root, manifest, directory)?
            .iter()
            .flatten()
            .any(|path| under_root_git_or_target(path)),
    )
}

/// `base` (絶対パス) に `path` をつないで、`.`・`..` を字面で畳む (root より上にも出られる)。
/// ファイルシステムの根より上に出るなら `None`。
fn absolute_lexical(base: &Path, path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in base.join(path).components() {
        match component {
            std::path::Component::Prefix(_) | std::path::Component::RootDir => {
                out.push(component.as_os_str());
            }
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            std::path::Component::Normal(part) => out.push(part),
        }
    }
    Some(out)
}

/// 絶対パス `path` が `root` の下なら、root 相対のパス (要素は大文字小文字を区別せずに比べる)。
/// Windows の `canonicalize` が付ける `\\?\` の有無は区別しない (`\\?\C:\x` と `C:\x` は同じ)。
fn relative_to_root(root: &Path, path: &Path) -> Option<PathBuf> {
    let key = |component: std::path::Component<'_>| match component {
        std::path::Component::Prefix(prefix) => match prefix.kind() {
            std::path::Prefix::VerbatimDisk(disk) | std::path::Prefix::Disk(disk) => {
                format!("{}:", char::from(disk))
            }
            std::path::Prefix::VerbatimUNC(server, share)
            | std::path::Prefix::UNC(server, share) => {
                format!(
                    r"\\{}\{}",
                    server.to_string_lossy(),
                    share.to_string_lossy()
                )
            }
            _ => component.as_os_str().to_string_lossy().into_owned(),
        },
        component => component.as_os_str().to_string_lossy().into_owned(),
    };
    let mut parts = path.components();
    for expected in root.components() {
        let actual = parts.next()?;
        if !key(actual).eq_ignore_ascii_case(&key(expected)) {
            return None;
        }
    }
    Some(parts.collect())
}

const DEPENDENCY_TABLES: [&str; 5] = [
    "dependencies",
    "dev-dependencies",
    "dev_dependencies",
    "build-dependencies",
    "build_dependencies",
];

/// root 相対の `base` に相対パス `path` をつなぎ、`.` と `..` を字句の上で畳む。絶対パスと、root の
/// 外へ出るパスは `None`。
fn lexical_join(base: &Path, path: &Path) -> Option<PathBuf> {
    let mut joined = Vec::new();
    for component in base.components().chain(path.components()) {
        match component {
            std::path::Component::Normal(part) => joined.push(part),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                joined.pop()?;
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => return None,
        }
    }
    Some(joined.into_iter().collect())
}

/// `include!("..")` (真) か `#[path = ".."]` (偽) の字面のパス。ほかの形 (`cfg_attr` の中・マクロの
/// token tree の中・escape を含む文字列・字面でない引数) は `None`。
fn literal_source_path(node: Node<'_>, source: &[u8]) -> Option<(bool, String)> {
    let (include, literal) = match node.kind() {
        "macro_invocation" => {
            if last_macro_identifier(node, source)? != "include" {
                return None;
            }
            let mut cursor = node.walk();
            let arguments = node
                .named_children(&mut cursor)
                .find(|child| child.kind() == "token_tree")?;
            let mut cursor = arguments.walk();
            let tokens: Vec<_> = arguments
                .named_children(&mut cursor)
                .filter(|token| !token.is_extra())
                .collect();
            let [literal] = tokens.as_slice() else {
                return None;
            };
            (true, *literal)
        }
        "attribute" => {
            let name = node.named_child(0)?;
            if name.kind() != "identifier" || name.utf8_text(source).ok()? != "path" {
                return None;
            }
            (false, node.child_by_field_name("value")?)
        }
        _ => return None,
    };
    if literal.kind() != "string_literal" {
        return None;
    }
    let text = literal
        .utf8_text(source)
        .ok()?
        .strip_prefix('"')?
        .strip_suffix('"')?;
    if text.contains('\\') {
        return None;
    }
    Some((include, text.to_owned()))
}

/// `include!` で読む `.rs` に `#[path]` (かそれを作り得る形) があるか。読めなければ真。
fn included_file_has_path_attribute(file: &Path) -> bool {
    let Some(source) = read_input_file(file) else {
        return true;
    };
    if memchr::memmem::find(&source, b"path").is_none() {
        return false;
    }
    let Ok(tree) = parser::parse_source(&source, LangId::Rust) else {
        return true;
    };
    crate::engine::tree_walk::any_node(
        tree.root_node(),
        crate::engine::tree_walk::Children::All,
        |node| {
            matches!(node.kind(), "attribute" | "token_tree" | "token_repetition")
                && crate::engine::rust_dynamic_source::node_includes_external_source(node, &source)
        },
    )
}

/// root の直下の `.git`・`target` (かその下) のパスか。
fn under_root_git_or_target(path: &Path) -> bool {
    path.components().next().is_some_and(|first| {
        first.as_os_str().to_str().is_some_and(|first| {
            first.eq_ignore_ascii_case(".git") || first.eq_ignore_ascii_case("target")
        })
    })
}

/// snapshot が書き出さなかった要素を、型を見てから root の直下の `.git`・`target` のディレクトリを
/// 読み飛ばす走査 (`scan_project_sources`・`scan_no_source_inclusion`) と同じく障壁にする。symlink は
/// 種類を問わず障壁で、gitlink は中を確かめられない (Issue #376)。
fn omissions_allow_typed_scan(root: &Path) -> bool {
    crate::git_support::tree_view(root)
        .omitted()
        .is_some_and(|omitted| {
            omitted.iter().all(|(path, kind)| {
                under_root_git_or_target(path)
                    && (path.components().count() > 1
                        || matches!(kind, crate::git_support::OmittedKind::Gitlink))
            })
        })
}

fn scan_sources<F>(root: &Path, mut source_is_uncertain: F) -> bool
where
    F: FnMut(&[u8]) -> bool,
{
    let Ok(root) = root.canonicalize() else {
        return false;
    };
    // root の直下の `.git`・`target` は名前で先に読み飛ばす。それ以外の、snapshot が書き出さなかった
    // 要素は、実際の checkout の symlink と同じく障壁にする (Issue #376)。
    if !crate::git_support::tree_view(&root)
        .omitted()
        .is_some_and(|omitted| {
            omitted
                .iter()
                .all(|(path, _)| under_root_git_or_target(path))
        })
    {
        return false;
    }
    for ancestor in root.ancestors().skip(1) {
        if ancestor.join("Cargo.toml").exists() || ancestor.join(".git").exists() {
            return false;
        }
    }
    let Some(manifest) = read_input_text_file(&root.join("Cargo.toml")) else {
        return false;
    };
    let Ok(manifest) = toml::from_str::<toml::Value>(&manifest) else {
        return false;
    };
    if manifest_has_source_paths(&manifest)
        || manifest
            .get("package")
            .and_then(|package| package.get("build"))
            .is_some_and(toml::Value::is_str)
        || root.join("build.rs").exists()
        || root.join("src/mod.rs").exists()
    {
        return false;
    }
    if manifest
        .get("package")
        .and_then(|package| package.get("workspace"))
        .is_some()
        || ["members", "default-members"].iter().any(|key| {
            manifest
                .get("workspace")
                .and_then(|workspace| workspace.get(key))
                .is_some_and(|value| {
                    value.as_array().is_none_or(|members| {
                        members.iter().any(|member| member.as_str() != Some("."))
                    })
                })
        })
    {
        return false;
    }
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            return false;
        };
        for entry in entries {
            let Ok(entry) = entry else {
                return false;
            };
            let path = entry.path();
            let name = entry.file_name();
            if directory == root.as_path() && matches!(name.to_str(), Some(".git" | "target")) {
                continue;
            }
            let Ok(kind) = entry.file_type() else {
                return false;
            };
            if kind.is_symlink() {
                return false;
            }
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file()
                && directory != root
                && name
                    .to_str()
                    .is_some_and(|name| name.eq_ignore_ascii_case("Cargo.toml"))
            {
                return false;
            } else if kind.is_file()
                && let Some(extension) = path.extension()
            {
                let Some(extension) = extension.to_str() else {
                    return false;
                };
                if !extension.is_ascii() {
                    return false;
                }
                if extension.eq_ignore_ascii_case("rs") {
                    let Some(source) = read_input_file(&path) else {
                        return false;
                    };
                    if source_is_uncertain(&source) {
                        return false;
                    }
                }
            }
        }
    }
    true
}

fn manifest_has_source_paths(value: &toml::Value) -> bool {
    match value {
        toml::Value::Table(table) => table
            .iter()
            .any(|(key, value)| key == "path" || manifest_has_source_paths(value)),
        toml::Value::Array(values) => values.iter().any(manifest_has_source_paths),
        _ => false,
    }
}

fn source_can_retarget_modules(source: &[u8]) -> bool {
    if memchr::memchr2(b'#', b'!', source).is_none() {
        return false;
    }
    let Ok(tree) = parser::parse_source(source, LangId::Rust) else {
        return true;
    };
    let root = tree.root_node();
    if root.has_error() {
        return true;
    }
    let mut shadowable_attributes = Vec::new();
    let mut imports = Vec::new();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if matches!(
            node.kind(),
            "macro_invocation" | "macro_definition" | "extern_crate_declaration"
        ) {
            return true;
        }
        if matches!(node.kind(), "attribute_item" | "inner_attribute_item") {
            let Some(names) = super::rust_std_origin::safe_attribute(node, source) else {
                return true;
            };
            shadowable_attributes.extend(names);
        }
        if node.kind() == "use_declaration" {
            let Ok(text) = node.utf8_text(source) else {
                return true;
            };
            imports.push(text);
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    shadowable_attributes.iter().any(|name| {
        imports
            .iter()
            .any(|import| import.contains('*') || import.contains(name))
    })
}

pub(super) fn is_allowed_expression_macro(name: &str) -> bool {
    matches!(
        name,
        "println"
            | "eprintln"
            | "print"
            | "eprint"
            | "assert"
            | "assert_eq"
            | "assert_ne"
            | "debug_assert"
            | "debug_assert_eq"
            | "debug_assert_ne"
            | "format"
            | "format_args"
            | "write"
            | "writeln"
            | "matches"
            | "panic"
            | "unreachable"
            | "todo"
            | "unimplemented"
            | "concat"
            | "stringify"
            | "include_str"
            | "include_bytes"
            | "env"
            | "option_env"
            | "file"
            | "line"
            | "column"
            | "module_path"
            | "vec"
            | "dbg"
    )
}

fn source_defines_allowed_macro(source: &[u8]) -> bool {
    if memchr::memmem::find(source, b"macro").is_none() {
        return false;
    }
    let Ok(tree) = parser::parse_source(source, LangId::Rust) else {
        return true;
    };
    let root = tree.root_node();
    if root.has_error() {
        return true;
    }
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if node.kind() == "macro_definition" {
            let Some(name) = node
                .child_by_field_name("name")
                .and_then(|name| name.utf8_text(source).ok())
            else {
                return true;
            };
            if is_allowed_expression_macro(name.strip_prefix("r#").unwrap_or(name)) {
                return true;
            }
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    false
}

/// Direct-import evidence can cross workspace members, but only when no
/// source in the repository can place the same file in another module.
fn scan_safe_private_import_environment(root: &Path) -> bool {
    scan_safe_source_environment(root, |source| {
        source_defines_allowed_macro(source) || source_may_include_elsewhere(source)
    })
}

/// crate 起点の直接 import の証明に使う環境。どのソースも crate の module の配置を変えられない
/// ことを確かめる。関数の本体のマクロは block の中の item しか作れず、crate 起点のパスの解決を
/// 変えないので、名前を問わず許す。関数の本体の外のマクロ、未知の属性、`extern crate` は拒み、
/// 標準の属性名 (`test`・std の derive) を import・マクロ定義・外部の glob で差し替えていない
/// ことも確かめる (差し替えた derive は任意の item を作れる)。配置を変えられないときは、関数の
/// 本体の外の `use` が束縛する名前を返す (import のパスの module 名の別名を見つけるため)。
fn scan_static_module_placement(root: &Path) -> Option<HashSet<String>> {
    static_placement_bindings(root, true)
}

fn static_placement_bindings(root: &Path, check_manifests: bool) -> Option<HashSet<String>> {
    let mut shadowable_names = HashSet::new();
    let mut defined_macros = HashSet::new();
    let mut imported_names = HashSet::new();
    let mut module_bindings = HashSet::new();
    let mut external_glob = false;
    let placed = scan_project_sources(root, check_manifests, |source| {
        // 属性とマクロは `#` か `!` を含む。`use` と `extern crate` は綴りで見つける。
        if memchr::memchr2(b'#', b'!', source).is_none()
            && memchr::memmem::find(source, b"use").is_none()
            && memchr::memmem::find(source, b"extern").is_none()
        {
            return false;
        }
        let Ok(tree) = parser::parse_source(source, LangId::Rust) else {
            return true;
        };
        let root = tree.root_node();
        if root.has_error() {
            return true;
        }
        let mut pending = vec![(root, false)];
        while let Some((node, in_body)) = pending.pop() {
            match node.kind() {
                "macro_invocation" if !in_body => return true,
                "extern_crate_declaration" => return true,
                "attribute_item" | "inner_attribute_item" => {
                    let Some(names) = super::rust_std_origin::safe_attribute(node, source) else {
                        return true;
                    };
                    shadowable_names.extend(names);
                }
                "macro_definition" => {
                    let Some(name) = node
                        .child_by_field_name("name")
                        .and_then(|name| name.utf8_text(source).ok())
                    else {
                        return true;
                    };
                    defined_macros.insert(name.strip_prefix("r#").unwrap_or(name).to_owned());
                }
                "use_declaration" => {
                    let Ok(text) = node.utf8_text(source) else {
                        return true;
                    };
                    let anchored = node
                        .child_by_field_name("argument")
                        .and_then(|value| value.utf8_text(source).ok())
                        .is_some_and(|text| {
                            ["crate::", "self::", "super::"]
                                .iter()
                                .any(|prefix| text.trim_start().starts_with(prefix))
                        });
                    external_glob |= text.contains('*') && !anchored;
                    imported_names.extend(
                        text.split(|ch: char| !ch.is_alphanumeric() && ch != '_')
                            .filter(|token| !token.is_empty())
                            .map(|token| token.strip_prefix("r#").unwrap_or(token).to_owned()),
                    );
                    if !in_body {
                        let Some(names) = use_names(node, source, true) else {
                            return true;
                        };
                        module_bindings.extend(names);
                    }
                }
                _ => {}
            }
            let in_body = in_body || matches!(node.kind(), "function_item" | "closure_expression");
            let mut cursor = node.walk();
            pending.extend(
                node.named_children(&mut cursor)
                    .map(|child| (child, in_body)),
            );
        }
        false
    });
    (placed
        && shadowable_names.iter().all(|name| {
            !external_glob && !defined_macros.contains(name) && !imported_names.contains(name)
        }))
    .then_some(module_bindings)
}

/// 公開し得る属性の付いた `macro_rules!` の名前をプロジェクト全体から集める。`#[macro_export]`
/// の定義は crate root の名前を差し替え、どのファイルにあっても届く (`cfg_attr` や未知の属性で
/// 付く場合も数え、`allow` や `doc` のような名前を公開しない属性は数えない)。それ以外の定義は、
/// 定義した module とその後の子にしか見えず、参照ファイルの祖先にあれば祖先の環境が拾う。
/// 依存の `path` はマクロの名前を crate root へ入れないので、manifest は見ない。解析できない
/// ソースがあれば `None`。
fn scan_macro_definitions(root: &Path) -> Option<HashSet<String>> {
    let mut names = HashSet::new();
    let scanned = scan_project_sources(root, false, |source| {
        if memchr::memmem::find(source, b"macro_rules").is_none() {
            return false;
        }
        let Ok(tree) = parser::parse_source(source, LangId::Rust) else {
            return true;
        };
        let root = tree.root_node();
        if root.has_error() {
            return true;
        }
        let mut pending = vec![root];
        while let Some(node) = pending.pop() {
            if node.kind() == "macro_definition" && transforming_scope_attribute(node, source) {
                let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|name| name.utf8_text(source).ok())
                else {
                    return true;
                };
                names.insert(name.strip_prefix("r#").unwrap_or(name).to_owned());
            }
            let mut cursor = node.walk();
            pending.extend(node.named_children(&mut cursor));
        }
        false
    });
    scanned.then_some(names)
}

/// 直下の宣言が `namespace` の名前を持つか。単位 struct は値の名前も持つ。タプル struct の
/// 値の名前 (コンストラクタ) はフィールドより広くは見えないので、全フィールドが `pub` のときだけ数える。
fn declares_in_namespace(node: Node<'_>, source: &[u8], namespace: ImportNamespace) -> bool {
    let value = match node.kind() {
        "function_item" | "const_item" | "static_item" => true,
        "struct_item" => node.child_by_field_name("body").is_none_or(|body| {
            body.kind() == "ordered_field_declaration_list" && public_tuple_fields(body, source)
        }),
        _ => false,
    };
    let type_name = matches!(
        node.kind(),
        "struct_item" | "enum_item" | "union_item" | "type_item" | "trait_item"
    );
    match namespace {
        ImportNamespace::Any => value || type_name,
        ImportNamespace::Value => value,
        ImportNamespace::Type => type_name,
    }
}

/// タプル struct の全フィールドが `pub` か。各フィールドは `type` フィールドの子で終わる。
fn public_tuple_fields(body: Node<'_>, source: &[u8]) -> bool {
    let mut cursor = body.walk();
    if !cursor.goto_first_child() {
        return false;
    }
    let mut public = false;
    loop {
        let child = cursor.node();
        if cursor.field_name() == Some("type") {
            if !public {
                return false;
            }
            public = false;
        } else if child.kind() == "visibility_modifier" {
            public = child.utf8_text(source).is_ok_and(|text| text == "pub");
        }
        if !cursor.goto_next_sibling() {
            return true;
        }
    }
}

/// 宣言を消し得る属性 (`cfg`・`test`・未知の属性) が付いていないか。消えた宣言の名前空間は
/// import で束縛されず、同名の参照が外側の宣言へ解決され得る。
fn unconditional_declaration(node: Node<'_>, source: &[u8]) -> bool {
    let mut previous = node.prev_named_sibling();
    while let Some(sibling) = previous {
        match sibling.kind() {
            "line_comment" | "block_comment" => {}
            "attribute_item" => {
                let name = sibling
                    .named_child(0)
                    .and_then(|attribute| attribute.named_child(0))
                    .and_then(|name| name.utf8_text(source).ok());
                if super::rust_std_origin::safe_attribute(sibling, source).is_none()
                    || matches!(name, Some("cfg" | "test"))
                {
                    return false;
                }
            }
            _ => break,
        }
        previous = sibling.prev_named_sibling();
    }
    true
}

fn scan_safe_source_environment(
    root: &Path,
    source_is_uncertain: impl FnMut(&[u8]) -> bool,
) -> bool {
    scan_project_sources(root, true, source_is_uncertain)
}

/// プロジェクトの Rust ソースを全部読む。`check_manifests` なら、ソースの置き場所を指定する
/// `path` キーのある Cargo.toml も拒む (依存の `path` も含む)。
fn scan_project_sources(
    root: &Path,
    check_manifests: bool,
    mut source_is_uncertain: impl FnMut(&[u8]) -> bool,
) -> bool {
    let Ok(root) = root.canonicalize() else {
        return false;
    };
    if !omissions_allow_typed_scan(&root) {
        return false;
    }
    for ancestor in root.ancestors().skip(1) {
        if ancestor.join("Cargo.toml").exists() || ancestor.join(".git").exists() {
            return false;
        }
    }
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            return false;
        };
        for entry in entries {
            let Ok(entry) = entry else {
                return false;
            };
            let Ok(kind) = entry.file_type() else {
                return false;
            };
            if kind.is_symlink() {
                return false;
            }
            let name = entry.file_name();
            if kind.is_dir() {
                if directory == root && matches!(name.to_str(), Some(".git" | "target")) {
                    continue;
                }
                pending.push(entry.path());
            } else if kind.is_file()
                && name
                    .to_str()
                    .is_some_and(|name| name.eq_ignore_ascii_case("Cargo.toml"))
            {
                if !check_manifests {
                    continue;
                }
                let Some(manifest) = read_input_text_file(&entry.path()) else {
                    return false;
                };
                let Ok(manifest) = toml::from_str::<toml::Value>(&manifest) else {
                    return false;
                };
                if manifest_has_source_paths(&manifest) {
                    return false;
                }
            } else if kind.is_file()
                && let Some(extension) = entry.path().extension()
            {
                let Some(extension) = extension.to_str() else {
                    return false;
                };
                if !extension.is_ascii() {
                    return false;
                }
                if extension.eq_ignore_ascii_case("rs") {
                    let Some(source) = read_input_file(&entry.path()) else {
                        return false;
                    };
                    if source_is_uncertain(&source) {
                        return false;
                    }
                }
            }
        }
    }
    true
}

fn scan_no_source_inclusion(root: &Path) -> bool {
    let Ok(root) = root.canonicalize() else {
        return false;
    };
    if !omissions_allow_typed_scan(&root) {
        return false;
    }
    for ancestor in root.ancestors().skip(1) {
        if ancestor.join("Cargo.toml").exists() || ancestor.join(".git").exists() {
            return false;
        }
    }
    let mut pending = vec![root.to_owned()];
    let mut sources = Vec::new();
    let mut manifests = Vec::new();
    let mut candidates = Vec::new();
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            return false;
        };
        for entry in entries {
            let Ok(entry) = entry else {
                return false;
            };
            let Ok(kind) = entry.file_type() else {
                return false;
            };
            if kind.is_symlink() {
                return false;
            }
            let name = entry.file_name();
            if kind.is_dir() {
                if directory == root && matches!(name.to_str(), Some(".git" | "target")) {
                    continue;
                }
                pending.push(entry.path());
            } else if kind.is_file()
                && name
                    .to_str()
                    .is_some_and(|name| name.eq_ignore_ascii_case("Cargo.toml"))
            {
                manifests.push(entry.path());
            } else if kind.is_file()
                && let Some(extension) = entry.path().extension()
            {
                let Some(extension) = extension.to_str() else {
                    return false;
                };
                if !extension.is_ascii() {
                    return false;
                }
                if extension.eq_ignore_ascii_case("rs") {
                    let Some(source) = read_input_file(&entry.path()) else {
                        return false;
                    };
                    sources.push((entry.path(), blake3::hash(&source)));
                    if source_has_include_macro(&source) {
                        let possible = super::rust_assert_include::candidate_offsets(&source);
                        if source_has_include_macro_except(&source, &possible) {
                            return false;
                        }
                        candidates.push((entry.path(), source));
                    }
                }
            }
        }
    }
    if candidates.is_empty() {
        return true;
    }
    let Some(proof) = super::rust_assert_include::AssertionProof::build(&sources, &manifests)
    else {
        return false;
    };
    candidates.into_iter().all(|(path, source)| {
        proof
            .receiver_offsets(&path, &source)
            .is_some_and(|offsets| !source_has_include_macro_except(&source, &offsets))
    })
}

fn source_has_include_macro(source: &[u8]) -> bool {
    source_has_include_macro_except(source, &HashSet::new())
}

fn source_has_include_macro_except(source: &[u8], receivers: &HashSet<usize>) -> bool {
    if memchr::memmem::find(source, b"include").is_none() {
        return false;
    }
    let Ok(tree) = parser::parse_source(source, LangId::Rust) else {
        return true;
    };
    let root = tree.root_node();
    if root.has_error() {
        return true;
    }
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if node.kind() == "identifier"
            && node
                .utf8_text(source)
                .ok()
                .is_some_and(|name| name.strip_prefix("r#").unwrap_or(name) == "include")
            && has_token_tree_ancestor(node)
            && !is_field_name_in_token_tree(node, source)
            && !receivers.contains(&node.start_byte())
        {
            return true;
        }
        if node.kind() == "macro_invocation"
            && last_macro_identifier(node, source).is_none_or(|name| name == "include")
        {
            return true;
        }
        if node.kind() == "use_declaration"
            && (node.utf8_text(source).is_err()
                || crate::engine::rust_dynamic_source::node_includes_external_source(node, source))
        {
            return true;
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    false
}

fn source_has_external_child_module(source: &[u8]) -> bool {
    let Ok(tree) = parser::parse_source(source, LangId::Rust) else {
        return true;
    };
    let root = tree.root_node();
    if root.has_error() {
        return true;
    }
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if node.kind() == "mod_item" && node.child_by_field_name("body").is_none() {
            return true;
        }
        if matches!(node.kind(), "attribute_item" | "inner_attribute_item")
            && super::rust_std_origin::safe_attribute(node, source).is_none()
        {
            return true;
        }
        if node.kind() == "macro_invocation"
            && (!has_function_ancestor(node)
                || !node
                    .child_by_field_name("macro")
                    .and_then(|name| name.utf8_text(source).ok())
                    .is_some_and(is_allowed_expression_macro))
        {
            return true;
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    false
}

/// Ignore ordinary imports here: a glob in a test module cannot relocate a
/// source file. Attributes and item macros can, so uncertain forms reject the
/// owner proof for every target in the repository.
fn source_may_include_elsewhere(source: &[u8]) -> bool {
    if memchr::memchr2(b'#', b'!', source).is_none() {
        return false;
    }
    let Ok(tree) = parser::parse_source(source, LangId::Rust) else {
        return true;
    };
    let root = tree.root_node();
    if root.has_error() {
        return true;
    }
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if matches!(node.kind(), "attribute_item" | "inner_attribute_item")
            && super::rust_std_origin::safe_attribute(node, source).is_none()
        {
            return true;
        }
        if node.kind() == "macro_invocation"
            && (!node
                .child_by_field_name("macro")
                .and_then(|name| name.utf8_text(source).ok())
                .is_some_and(is_allowed_expression_macro)
                || !has_function_ancestor(node))
        {
            return true;
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    false
}

/// Module privacy only needs to rule out alternate file inclusion. Ordinary
/// expression macros cannot create a sibling module, while item macros can.
fn source_can_retarget_private_scope(source: &[u8]) -> bool {
    if memchr::memchr2(b'#', b'!', source).is_none() {
        return false;
    }
    let Ok(tree) = parser::parse_source(source, LangId::Rust) else {
        return true;
    };
    let root = tree.root_node();
    if root.has_error() {
        return true;
    }
    let mut shadowable_attributes = Vec::new();
    let mut allowed_macros = Vec::new();
    let mut defined_macros = HashSet::new();
    let mut imports = Vec::new();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        match node.kind() {
            "macro_invocation" => {
                let mut ancestor = node.parent();
                let mut expression_context = false;
                while let Some(parent) = ancestor {
                    if matches!(parent.kind(), "function_item" | "closure_expression") {
                        expression_context = true;
                        break;
                    }
                    if matches!(parent.kind(), "source_file" | "mod_item") {
                        break;
                    }
                    ancestor = parent.parent();
                }
                if !expression_context {
                    return true;
                }
                // A user macro can expand to local items, including include!.
                // These built-in expression macros do not retarget source files.
                let name = node
                    .child_by_field_name("macro")
                    .and_then(|name| name.utf8_text(source).ok());
                if !name.is_some_and(is_allowed_expression_macro) {
                    return true;
                }
                allowed_macros.push(name.unwrap());
            }
            "macro_definition" => {
                let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|name| name.utf8_text(source).ok())
                else {
                    return true;
                };
                defined_macros.insert(name);
            }
            "extern_crate_declaration" => return true,
            "attribute_item" | "inner_attribute_item" => {
                let Some(names) = super::rust_std_origin::safe_attribute(node, source) else {
                    return true;
                };
                shadowable_attributes.extend(names);
            }
            "use_declaration" => {
                let Ok(text) = node.utf8_text(source) else {
                    return true;
                };
                imports.push(text);
            }
            _ => {}
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    shadowable_attributes
        .iter()
        .map(String::as_str)
        .chain(allowed_macros)
        .any(|name| {
            defined_macros.contains(name)
                || imports
                    .iter()
                    .any(|import| import.contains('*') || import.contains(name))
        })
}

pub(super) fn has_uncertain_ancestor(mut node: Node<'_>) -> bool {
    while let Some(parent) = node.parent() {
        if matches!(
            parent.kind(),
            "token_tree" | "attribute_item" | "inner_attribute_item" | "use_declaration"
        ) || (parent.kind() == "mod_item" && parent.child_by_field_name("body").is_some())
        {
            return true;
        }
        node = parent;
    }
    false
}

pub(super) fn has_attribute_prefix(node: Node<'_>) -> bool {
    let mut previous = node.prev_named_sibling();
    while let Some(sibling) = previous {
        if matches!(sibling.kind(), "line_comment" | "block_comment") {
            previous = sibling.prev_named_sibling();
        } else {
            return sibling.kind() == "attribute_item";
        }
    }
    false
}

pub(super) fn scope_can_rebind_name(scope: Node<'_>, source: &[u8], name: &str) -> bool {
    let mut cursor = scope.walk();
    let mut pending = scope.named_children(&mut cursor).collect::<Vec<_>>();
    while let Some(node) = pending.pop() {
        if matches!(node.kind(), "macro_invocation" | "use_declaration") {
            return true;
        }
        let possible_shadow = matches!(
            node.kind(),
            "struct_item" | "enum_item" | "union_item" | "type_item" | "trait_item"
        );
        if possible_shadow
            && node
                .child_by_field_name("name")
                .and_then(|child| child.utf8_text(source).ok())
                == Some(name)
        {
            return true;
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    false
}

pub(super) fn scope_can_rebind_import_name(scope: Node<'_>, source: &[u8], name: &str) -> bool {
    scope_can_rebind_import_name_beside(scope, source, name, None)
}

/// `proven_macro` は標準のマクロと証明した呼び出しの id。その呼び出しだけは文の位置でも
/// 束縛を作らないとみなす。
fn scope_can_rebind_import_name_beside(
    scope: Node<'_>,
    source: &[u8],
    name: &str,
    proven_macro: Option<usize>,
) -> bool {
    let mut cursor = scope.walk();
    let mut pending = if scope.kind() == "match_arm" {
        vec![scope]
    } else {
        scope.named_children(&mut cursor).collect::<Vec<_>>()
    };
    while let Some(node) = pending.pop() {
        // arm のパターン・ガードの束縛はその arm 内だけに届く。参照を囲む arm は別途検査する。
        if node.kind() == "match_arm" && node != scope {
            continue;
        }
        if node.kind() == "use_declaration" {
            return true;
        }
        if node.kind() == "macro_invocation" {
            // 式位置の展開は外側へ束縛を追加できない。文・item 位置は保留する。
            if Some(node.id()) != proven_macro
                && node.parent().is_none_or(|parent| {
                    matches!(
                        parent.kind(),
                        "source_file" | "block" | "expression_statement"
                    )
                })
            {
                return true;
            }
            continue;
        }
        let pattern = match node.kind() {
            "let_declaration" | "let_condition" | "parameter" | "match_arm" | "for_expression" => {
                node.child_by_field_name("pattern")
            }
            "closure_parameters" => Some(node),
            "const_parameter" => node.child_by_field_name("name"),
            _ => None,
        };
        if pattern.is_some_and(|pattern| {
            crate::engine::tree_walk::any_node(
                pattern,
                crate::engine::tree_walk::Children::Named,
                |node| {
                    node.kind() == "macro_invocation"
                        || (matches!(node.kind(), "identifier" | "shorthand_field_identifier")
                            && node.utf8_text(source).ok().is_none_or(|text| {
                                crate::language::normalize_identifier(LangId::Rust, text) == name
                            }))
                },
            )
        }) {
            return true;
        }
        if matches!(
            node.kind(),
            "function_item"
                | "function_signature_item"
                | "const_item"
                | "static_item"
                | "type_item"
                | "struct_item"
                | "enum_item"
                | "union_item"
        ) && node
            .child_by_field_name("name")
            .and_then(|child| child.utf8_text(source).ok())
            .is_some_and(|item| crate::language::normalize_identifier(LangId::Rust, item) == name)
        {
            return true;
        }
        if matches!(
            node.kind(),
            "block"
                | "function_item"
                | "closure_expression"
                | "impl_item"
                | "trait_item"
                | "mod_item"
                | "const_item"
                | "static_item"
                | "macro_definition"
        ) {
            continue;
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    false
}

pub(super) fn path_segments(node: Node<'_>, source: &[u8]) -> Option<Vec<String>> {
    // `a::b::c` は `path` 側へ入れ子になる。段数だけ再帰しないよう、`path` を先頭まで
    // たどって末尾側から名前を集め、最後に並べ直す。
    let mut reversed = Vec::new();
    let mut current = node;
    loop {
        match current.kind() {
            "identifier" | "type_identifier" | "crate" | "self" | "super" => {
                reversed.push(current.utf8_text(source).ok()?.to_owned());
                break;
            }
            "scoped_identifier" | "scoped_type_identifier" => {
                let path = current.child_by_field_name("path")?;
                reversed.push(
                    current
                        .child_by_field_name("name")?
                        .utf8_text(source)
                        .ok()?
                        .to_owned(),
                );
                current = path;
            }
            _ => return None,
        }
    }
    reversed.reverse();
    Some(reversed)
}

fn plain_crate_import_path(text: &str) -> Option<Vec<String>> {
    let path = text.trim().strip_prefix("use ")?.strip_suffix(';')?.trim();
    let parts = path.split("::").collect::<Vec<_>>();
    if parts.len() < 3 || parts[0] != "crate" {
        return None;
    }
    if parts.iter().any(|part| {
        part.is_empty()
            || !part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    }) {
        return None;
    }
    Some(parts.into_iter().map(str::to_owned).collect())
}

pub(super) fn source_module_path(
    dir: &Path,
    ref_file: &Path,
) -> Option<(PathBuf, PathBuf, Vec<String>)> {
    source_module_path_inner(dir, ref_file, false)
}

fn source_module_path_inner(
    dir: &Path,
    ref_file: &Path,
    allow_nested_root_names: bool,
) -> Option<(PathBuf, PathBuf, Vec<String>)> {
    // 最も近い manifest を、snapshot が書き出さなかった要素で取り違えない (Issue #376)。
    let mut package = None;
    for ancestor in ref_file.ancestors() {
        if checked_is_file(dir, &ancestor.join("Cargo.toml"))? {
            package = Some(ancestor);
            break;
        }
    }
    let package = package?;
    let manifest = read_input_text_file(&dir.join(package).join("Cargo.toml"))?;
    let manifest_value = toml::from_str::<toml::Value>(&manifest).ok()?;
    let edition = manifest_value.get("package")?.get("edition")?.as_str()?;
    if !matches!(edition, "2018" | "2021" | "2024") {
        return None;
    }
    let targets = RustTargets::parse(&manifest, package)?;
    if !targets.has_package || !targets.roots_complete || targets.has_explicit_roots() {
        return None;
    }
    let src = package.join("src");
    let relative = ref_file.strip_prefix(&src).ok()?;
    let parts = relative
        .components()
        .map(|part| part.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()?;
    let (folders, file) = parts.split_last().map(|(file, folders)| (folders, *file))?;
    if folders.contains(&"bin") {
        return None;
    }
    let mut modules = folders
        .iter()
        .map(|part| (*part).to_owned())
        .collect::<Vec<_>>();
    if file != "mod.rs"
        && ((!matches!(file, "lib.rs" | "main.rs"))
            || (allow_nested_root_names && !folders.is_empty()))
    {
        modules.push(file.strip_suffix(".rs")?.to_owned());
    } else if matches!(file, "lib.rs" | "main.rs") && !folders.is_empty() {
        return None;
    }
    Some((package.to_owned(), src, modules))
}

/// Follow ordinary `mod name;` declarations without the owner resolver's
/// restrictions on unrelated imports, attributes, and expression macros.
pub(super) fn private_module_file(
    dir: &Path,
    src: &Path,
    modules: &[String],
    verify_syntax: bool,
) -> Option<PathBuf> {
    private_module_file_from_roots(
        dir,
        src,
        modules,
        verify_syntax,
        false,
        [src.join("lib.rs"), src.join("main.rs")],
    )
}

fn private_module_file_from_roots(
    dir: &Path,
    src: &Path,
    modules: &[String],
    verify_syntax: bool,
    normalize_raw_names: bool,
    roots: impl IntoIterator<Item = PathBuf>,
) -> Option<PathBuf> {
    let mut resolved = None;
    for root in roots {
        if !checked_is_file(dir, &root)? {
            continue;
        }
        let mut parent = root;
        let mut prefix = PathBuf::new();
        let mut valid = true;
        for module in modules {
            let source = read_input_file(&dir.join(&parent))?;
            if verify_syntax && source_can_retarget_private_scope(&source) {
                return None;
            }
            let tree = parser::parse_source(&source, LangId::Rust).ok()?;
            let node = tree.root_node();
            if node.has_error() {
                return None;
            }
            let mut cursor = node.walk();
            let declarations = node
                .named_children(&mut cursor)
                .filter(|child| child.kind() == "mod_item")
                .filter(|child| {
                    child
                        .child_by_field_name("name")
                        .and_then(|name| name.utf8_text(&source).ok())
                        .map(|name| {
                            if normalize_raw_names {
                                name.strip_prefix("r#").unwrap_or(name)
                            } else {
                                name
                            }
                        })
                        == Some(module.as_str())
                        && child.child_by_field_name("body").is_none()
                })
                .count();
            if declarations != 1 {
                valid = false;
                break;
            }
            let base = src.join(&prefix);
            let file = base.join(format!("{module}.rs"));
            let nested = base.join(module).join("mod.rs");
            parent = match (checked_is_file(dir, &file)?, checked_is_file(dir, &nested)?) {
                (true, false) => file,
                (false, true) => nested,
                _ => return None,
            };
            prefix.push(module);
        }
        if valid {
            match &resolved {
                Some(previous) if previous != &parent => return None,
                _ => resolved = Some(parent),
            }
        }
    }
    resolved
}

fn transforming_scope_attribute(node: Node<'_>, source: &[u8]) -> bool {
    let mut previous = node.prev_named_sibling();
    while let Some(attribute) = previous {
        match attribute.kind() {
            "line_comment" | "block_comment" => {}
            "attribute_item" => {
                if super::rust_std_origin::safe_attribute(attribute, source)
                    .is_none_or(|names| !names.is_empty())
                {
                    return true;
                }
            }
            _ => break,
        }
        previous = attribute.prev_named_sibling();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::{
        FileEvidence, ImportNamespace, RustPathOwnerIndex, RustPathOwnerProjectProof,
        declares_in_namespace, module_wiring, private_module_wiring, scan_macro_definitions,
        scan_ordinary_sources, scan_safe_private_import_environment, scan_static_module_placement,
        source_can_retarget_modules, source_can_retarget_private_scope,
        source_defines_allowed_macro, source_has_external_child_module, source_has_include_macro,
        source_may_include_elsewhere, unconditional_declaration,
    };
    use crate::engine::parser;
    use crate::language::LangId;

    /// `HEAD` という名前のファイルがあるだけの普通のディレクトリ (`target/tools/HEAD`) は、Git の
    /// ディレクトリの形にしない (その `objects` の下の package を読み飛ばさない)。
    #[test]
    fn git_directory_shape_requires_head_config_and_refs() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path();
        std::fs::write(directory.join("HEAD"), "").unwrap();
        assert!(!super::git_directory_shape(directory));
        std::fs::write(directory.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        assert!(!super::git_directory_shape(directory));
        // `config`・`refs`・`objects` のどれが欠けても形にしない。
        std::fs::write(directory.join("config"), "[core]\n").unwrap();
        assert!(!super::git_directory_shape(directory));
        std::fs::create_dir(directory.join("refs")).unwrap();
        assert!(!super::git_directory_shape(directory));
        std::fs::create_dir(directory.join("objects")).unwrap();
        assert!(super::git_directory_shape(directory));
        std::fs::remove_file(directory.join("config")).unwrap();
        assert!(!super::git_directory_shape(directory));
        std::fs::write(directory.join("config"), "[core]\n").unwrap();
        std::fs::remove_dir(directory.join("refs")).unwrap();
        assert!(!super::git_directory_shape(directory));
        std::fs::create_dir(directory.join("refs")).unwrap();
        assert!(super::git_directory_shape(directory));
        std::fs::write(directory.join("HEAD"), "ref: not a ref\n").unwrap();
        assert!(!super::git_directory_shape(directory));
        std::fs::write(directory.join("HEAD"), format!("{}\n", "a".repeat(40))).unwrap();
        assert!(super::git_directory_shape(directory));
        std::fs::write(directory.join("HEAD"), "not a ref\n").unwrap();
        assert!(!super::git_directory_shape(directory));
    }

    #[test]
    fn rust_private_function_inline_test_wiring_ignores_call_updates() {
        let before = b"fn exists(_: &str) {} #[cfg(test)] mod tests { use super::*; #[test] fn sample() { assert!(exists(\"x\")); } }";
        let after = b"fn exists(_: &Packet) {} #[cfg(test)] mod tests { use super::*; #[test] fn sample() { assert!(exists(&Packet { data: vec![1] })); } }";
        assert_eq!(
            private_module_wiring(before).unwrap(),
            private_module_wiring(after).unwrap()
        );
        assert_ne!(
            module_wiring(before).unwrap(),
            module_wiring(after).unwrap()
        );
    }

    #[test]
    fn rust_private_function_inline_test_wiring_preserves_boundaries() {
        for (before, after) in [
            ("mod a { use crate::one; }", "mod a {} use crate::one;"),
            ("mod a { mod b; }", "mod a {} mod b;"),
            (
                "mod a { #[cfg(test)] fn f() {} }",
                "mod a {} #[cfg(test)] fn f() {}",
            ),
            ("#[cfg(test)] mod a {}", "#[cfg(unix)] mod a {}"),
            ("mod a {}", "mod a;"),
            ("mod a { use crate::one; }", "mod a { use crate::two; }"),
            (
                "mod a { fn f() { custom!(one); } }",
                "mod a { fn f() { custom!(two); } }",
            ),
            (
                "mod a { fn f() { include!(\"one.rs\"); } }",
                "mod a { fn f() { include!(\"two.rs\"); } }",
            ),
            (
                "mod a { macro_rules! m { () => { one } } }",
                "mod a { macro_rules! m { () => { two } } }",
            ),
        ] {
            assert_ne!(
                private_module_wiring(before.as_bytes()).unwrap(),
                private_module_wiring(after.as_bytes()).unwrap(),
                "{before} -> {after}"
            );
        }
    }

    /// 関数の本体のマクロは名前を問わず許し、crate の module の配置を変え得る構文だけを拒む。
    #[test]
    fn static_module_placement_accepts_only_function_body_macros() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src/bin")).unwrap();
        std::fs::create_dir_all(root.path().join("tests")).unwrap();
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(root.path().join("src/lib.rs"), "mod a;\nmod b;\n").unwrap();
        std::fs::write(root.path().join("src/a.rs"), "fn helper() {}\n").unwrap();
        let body_macros = "use crate::a::{self as renamed, helper};\npub fn row(id: i64) -> Vec<i64> { params![id] }\npub fn more() { use crate::a as local; thing!(); let f = || custom!(1); include!(\"a.rs\"); }\n#[derive(Clone, Debug)]\nstruct Plain;\n#[cfg(test)]\nmod tests { use super::*; #[test] fn works() { assert!(true); } }\n";
        std::fs::write(root.path().join("src/b.rs"), body_macros).unwrap();
        // 関数の本体の外の `use` が束縛する名前だけを返す (glob と関数の本体の `use` は数えない)。
        let bindings = scan_static_module_placement(root.path()).unwrap();
        assert!(bindings.contains("renamed") && bindings.contains("helper"));
        assert!(!bindings.contains("local") && !bindings.contains("a"));
        // 従来の環境は関数の本体の標準でないマクロも拒む。
        assert!(!scan_safe_private_import_environment(root.path()));

        for (path, source) in [
            ("src/b.rs", "make_items!();\n"),
            ("src/b.rs", "struct S;\nimpl S { make_items!(); }\n"),
            ("src/b.rs", "const X: () = { make_items!(); };\n"),
            ("src/b.rs", "#[doc = include_str!(\"a.rs\")]\nfn f() {}\n"),
            ("src/b.rs", "#[path = \"a.rs\"]\nmod moved;\n"),
            ("src/b.rs", "#[custom]\nfn f() {}\n"),
            // `#`・`!`・`use` を含まない宣言も事前の絞り込みで落とさない。
            ("src/b.rs", "extern crate alloc;\n"),
            ("src/b.rs", "fn f() { thing!( }\n"),
            (
                "src/b.rs",
                "use external::Relocate as Clone;\n#[derive(Clone)]\nstruct S;\n",
            ),
            ("src/b.rs", "use external::*;\n#[test]\nfn works() {}\n"),
            (
                "src/b.rs",
                "macro_rules! test { () => {} }\n#[test]\nfn works() {}\n",
            ),
            ("src/bin/tool.rs", "include!(\"../a.rs\");\n"),
            (
                "src/bin/tool.rs",
                "std /*a*/ :: /*b*/ include!{\"../a.rs\"}\n",
            ),
            ("tests/relocate.rs", "#[path = \"../src/a.rs\"]\nmod a;\n"),
        ] {
            std::fs::write(root.path().join(path), source).unwrap();
            assert!(
                scan_static_module_placement(root.path()).is_none(),
                "{path}: {source}"
            );
            if path == "src/b.rs" {
                std::fs::write(root.path().join(path), body_macros).unwrap();
            } else {
                std::fs::remove_file(root.path().join(path)).unwrap();
            }
        }
        assert!(scan_static_module_placement(root.path()).is_some());
        // 入れ子の manifest の `path` と、祖先の Cargo プロジェクトは従来の環境と同じく拒む。
        std::fs::create_dir_all(root.path().join("other")).unwrap();
        std::fs::write(
            root.path().join("other/cargo.toml"),
            "[lib]\npath = \"a.rs\"\n",
        )
        .unwrap();
        assert!(scan_static_module_placement(root.path()).is_none());
        std::fs::remove_file(root.path().join("other/cargo.toml")).unwrap();
        assert!(scan_static_module_placement(&root.path().join("src")).is_none());
    }

    /// `#[macro_export]` は crate root の名前を差し替えるので、公開し得る属性の付いた定義を
    /// 置き場所を問わず集める。それ以外の定義は定義した module の字句スコープにとどまる。
    #[test]
    fn macro_definitions_are_collected_from_every_source() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src/nested")).unwrap();
        std::fs::create_dir_all(root.path().join("helper/src")).unwrap();
        // 依存の `path` はマクロの名前を crate root へ入れないので、走査を止めない。
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nhelper = { path = \"helper\" }\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("helper/Cargo.toml"),
            "[package]\nname = \"helper\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(root.path().join("helper/src/lib.rs"), "").unwrap();
        std::fs::write(root.path().join("src/lib.rs"), "mod store;\n").unwrap();
        std::fs::write(
            root.path().join("src/store.rs"),
            "#[macro_export]\nmacro_rules! format { ($s:literal) => { 1 }; }\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("src/nested/deep.rs"),
            "macro_rules! println { () => {} }\n#[allow(unused_macros)]\n#[doc(hidden)]\nmacro_rules! write { () => {} }\nfn f() {\n    #[cfg_attr(test, macro_export)]\n    /// doc\n    macro_rules! r#local { () => {} }\n}\n",
        )
        .unwrap();
        let names = scan_macro_definitions(root.path()).unwrap();
        assert!(
            names.contains("format") && names.contains("local"),
            "{names:?}"
        );
        assert!(
            !names.contains("println") && !names.contains("write"),
            "{names:?}"
        );
        std::fs::write(
            root.path().join("src/nested/deep.rs"),
            "macro_rules! broken { (\n",
        )
        .unwrap();
        assert!(scan_macro_definitions(root.path()).is_none());
    }

    /// import は宣言のある名前空間だけを束縛する。`cfg` などで消え得る宣言は束縛の根拠にしない。
    #[test]
    fn import_targets_need_an_unconditional_declaration_in_the_namespace() {
        let source = b"pub const C: u8 = 1;\npub static S: u8 = 1;\npub fn f() {}\npub struct Unit;\npub struct Tuple(u8);\npub struct Open(pub u8, #[allow(unused)] pub u16);\npub struct Mixed(pub u8, pub(crate) u16);\npub struct Empty();\npub struct Named { x: u8 }\npub enum E { A }\npub union U { x: u8 }\npub type T = u8;\npub trait Tr {}\n#[cfg(feature = \"x\")]\npub const GATED: u8 = 1;\n#[test]\nfn tested() {}\n#[derive(Clone)]\n/// doc\npub struct Derived;\n#[inline]\npub fn inlined() {}\n#[custom]\npub fn custom() {}\n";
        let tree = parser::parse_source(source, LangId::Rust).unwrap();
        let root = tree.root_node();
        let mut cursor = root.walk();
        let declarations = root
            .named_children(&mut cursor)
            .filter_map(|node| {
                let name = node.child_by_field_name("name")?.utf8_text(source).ok()?;
                Some((name.to_owned(), node))
            })
            .collect::<std::collections::HashMap<_, _>>();
        for (name, value, type_name) in [
            ("C", true, false),
            ("S", true, false),
            ("f", true, false),
            ("Unit", true, true),
            // タプル struct のコンストラクタはフィールドより広くは見えない。
            ("Tuple", false, true),
            ("Open", true, true),
            ("Mixed", false, true),
            ("Empty", true, true),
            ("Named", false, true),
            ("E", false, true),
            ("U", false, true),
            ("T", false, true),
            ("Tr", false, true),
        ] {
            let node = declarations[name];
            assert_eq!(
                declares_in_namespace(node, source, ImportNamespace::Value),
                value,
                "{name}"
            );
            assert_eq!(
                declares_in_namespace(node, source, ImportNamespace::Type),
                type_name,
                "{name}"
            );
            assert!(
                declares_in_namespace(node, source, ImportNamespace::Any),
                "{name}"
            );
            assert!(unconditional_declaration(node, source), "{name}");
        }
        for (name, unconditional) in [
            ("GATED", false),
            ("tested", false),
            ("Derived", true),
            ("inlined", true),
            ("custom", false),
        ] {
            assert_eq!(
                unconditional_declaration(declarations[name], source),
                unconditional,
                "{name}"
            );
        }
    }

    #[test]
    fn import_namespace_grammar_nodes_and_fields_exist() {
        let language = LangId::Rust.ts_language();
        for query in [
            "(struct_item name: (_) body: (_))",
            "(const_item name: (_))",
            "(static_item name: (_))",
            "(function_item name: (_))",
            "(enum_item name: (_))",
            "(union_item name: (_))",
            "(type_item name: (_))",
            "(trait_item name: (_))",
            "(closure_expression)",
            "(extern_crate_declaration)",
            "(macro_definition name: (_))",
            "(use_declaration argument: (_))",
            "(inner_attribute_item)",
            "(attribute_item (attribute (identifier)))",
        ] {
            tree_sitter::Query::new(&language, query).unwrap();
        }
    }

    /// staged の snapshot (marker と記録だけを置いた形) の Cargo のプロジェクト。`entries` は記録の要素。
    fn snapshot_project(entries: &str) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        std::fs::create_dir_all(root.path().join(".git")).unwrap();
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(root.path().join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        crate::git_support::fake_snapshot_record(root.path(), entries);
        root
    }

    /// snapshot が書き出さなかった要素は、実際の checkout の symlink と同じく全体の走査を止め、その
    /// パスのファイルの有無も決めない (Issue #376)。
    #[test]
    fn snapshot_omissions_block_scans_and_existence_checks() {
        // 対照: 要素の無い記録と、root の直下の `target` の中だけの要素。
        for entries in [
            "",
            "{\"path\":\"target/debug/link\",\"kind\":\"symlink\",\"target\":\"unknown\"}",
        ] {
            let root = snapshot_project(entries);
            assert!(scan_ordinary_sources(root.path()), "{entries}");
            assert!(super::scan_no_source_inclusion(root.path()), "{entries}");
            assert_eq!(
                super::checked_is_file(root.path(), std::path::Path::new("src/lib.rs")),
                Some(true)
            );
        }
        // 種類を問わない symlink と gitlink は、実際の checkout と同じく障壁。root の直下の `target`
        // 自身が symlink なら、型を先に見る走査では障壁。
        for entries in [
            "{\"path\":\"CLAUDE.md\",\"kind\":\"symlink\",\"target\":\"regular_file\"}",
            "{\"path\":\"src/main.rs\",\"kind\":\"symlink\",\"target\":\"regular_file\"}",
            "{\"path\":\"vendor/sub\",\"kind\":\"gitlink\"}",
        ] {
            let root = snapshot_project(entries);
            assert!(!scan_ordinary_sources(root.path()), "{entries}");
            assert!(!super::scan_no_source_inclusion(root.path()), "{entries}");
        }
        let root =
            snapshot_project("{\"path\":\"target\",\"kind\":\"symlink\",\"target\":\"directory\"}");
        assert!(scan_ordinary_sources(root.path()));
        assert!(!super::scan_no_source_inclusion(root.path()));
        // 書き出さなかった第 2 の crate root と、gitlink の下のファイルの有無は決めない。書き出さなかった
        // symlink のディレクトリで開けない名前 (`src/other.rs`) も、その別名でありうるので決めない。
        let root = snapshot_project(
            "{\"path\":\"src/main.rs\",\"kind\":\"symlink\",\"target\":\"regular_file\"},{\"path\":\"vendor/sub\",\"kind\":\"gitlink\"}",
        );
        for path in ["src/main.rs", "src/other.rs", "vendor/sub/Cargo.toml"] {
            assert_eq!(
                super::checked_is_file(root.path(), std::path::Path::new(path)),
                None,
                "{path}"
            );
        }
        // 対照: 開けるファイルと、書き出さなかった symlink の無いディレクトリで開けない名前。
        for (path, exists) in [
            ("Cargo.toml", true),
            ("src/lib.rs", true),
            ("vendor/other.rs", false),
            ("src/lib.rs/x.rs", false),
        ] {
            assert_eq!(
                super::checked_is_file(root.path(), std::path::Path::new(path)),
                Some(exists),
                "{path}"
            );
        }
        assert_eq!(
            super::private_module_file(root.path(), std::path::Path::new("src"), &[], false),
            None
        );
        // `src/main.rs` として開けうる別名の symlink: 大文字小文字だけが違う名前と、NTFS で短い名前 (8.3)
        // を `main.rs` にできる名前。
        for link in ["src/MAIN.rs", "src/bootstrap-link.rs"] {
            let root = snapshot_project(&format!(
                "{{\"path\":\"{link}\",\"kind\":\"symlink\",\"target\":\"regular_file\"}}"
            ));
            assert_eq!(
                super::checked_is_file(root.path(), std::path::Path::new("src/main.rs")),
                None,
                "{link}"
            );
            assert_eq!(
                super::private_module_file(root.path(), std::path::Path::new("src"), &[], false),
                None,
                "{link}"
            );
        }
        // 対照: 文書の symlink だけなら、`src` の crate root を決められる。
        let root = snapshot_project(
            "{\"path\":\"CLAUDE.md\",\"kind\":\"symlink\",\"target\":\"regular_file\"}",
        );
        assert_eq!(
            super::checked_is_file(root.path(), std::path::Path::new("src/main.rs")),
            Some(false)
        );
        assert_eq!(
            super::private_module_file(root.path(), std::path::Path::new("src"), &[], false),
            Some(std::path::PathBuf::from("src/lib.rs"))
        );
    }

    /// プロジェクト単位の証明は Rust ソースを全部読む。100MB の入力上限を超えるファイル
    /// (gitignore 済みの生成物でも `read_dir` で見つかる) は読まず、内容を確かめられない
    /// として証明を止める。旧実装は上限なしで読み、1 ファイルでメモリが際限なく増えていた。
    #[test]
    fn project_scan_does_not_read_sources_over_the_input_limit() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        std::fs::create_dir_all(root.path().join("out")).unwrap();
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(root.path().join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        let generated = root.path().join("out/generated.rs");
        std::fs::write(&generated, "pub fn generated() {}\n").unwrap();
        // 対照: 上限以内のファイルは従来どおり読んで判定する。
        assert!(scan_ordinary_sources(root.path()));
        // ディスクを消費しない疎ファイルで上限を 1 byte 超えさせる。
        std::fs::OpenOptions::new()
            .write(true)
            .open(&generated)
            .and_then(|file| file.set_len(crate::commands::MAX_INPUT_SIZE as u64 + 1))
            .unwrap();
        assert!(!scan_ordinary_sources(root.path()));
    }

    #[test]
    fn type_alias_does_not_prove_an_independent_type_owner() {
        let source = b"type Alias = crate::origin::Alias;\nstruct Local;\n";
        let tree = parser::parse_source(source, LangId::Rust).unwrap();
        let evidence = FileEvidence::collect(tree.root_node(), source).unwrap();
        assert!(!evidence.types.contains("Alias"));
        assert!(evidence.types.contains("Local"));
    }

    #[test]
    fn imported_attribute_name_blocks_module_owner_proof() {
        assert!(source_can_retarget_modules(
            b"use macros::test;\n#[test]\nfn sample() {}\n"
        ));
        assert!(source_can_retarget_modules(
            b"use macros::Debug;\n#[derive(Debug)]\nstruct Sample;\n"
        ));
        assert!(source_can_retarget_modules(
            b"use macros::derive;\n#[derive(Debug)]\nstruct Sample;\n"
        ));
        assert!(!source_can_retarget_modules(
            b"#[derive(Debug)]\nstruct Sample;\n"
        ));
    }

    #[test]
    fn private_scope_allows_standard_expression_macros_but_not_shadowed_macros() {
        assert!(!source_can_retarget_private_scope(
            b"#[cfg(test)] mod tests;\nfn call() { println!(\"x\"); assert!(true); }\n"
        ));
        assert!(source_can_retarget_private_scope(
            b"use custom::println;\nfn call() { println!(\"x\"); }\n"
        ));
        assert!(source_can_retarget_private_scope(
            b"macro_rules! println { () => {} }\nfn call() { println!(); }\n"
        ));
        assert!(source_can_retarget_private_scope(b"make_modules!();\n"));
        assert!(source_defines_allowed_macro(
            b"macro_rules! dbg { () => {} }\n"
        ));
        assert!(!source_defines_allowed_macro(
            b"macro_rules! local { () => {} }\n"
        ));
    }

    #[test]
    fn private_scope_rejects_builtin_macro_shadowed_in_another_file() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("src/lib.rs"),
            "macro_rules! dbg { ($value:expr) => { $value }; }\nmod child;\n",
        )
        .unwrap();
        std::fs::write(root.path().join("src/child.rs"), "fn call() { dbg!(1); }\n").unwrap();
        let proof = RustPathOwnerProjectProof::new(root.path(), true, true, true, &[], "");
        assert!(!proof.has_safe_private_import_environment());
        assert!(!proof.allows_private_scope_paths());
    }

    #[test]
    fn private_import_environment_rejects_other_target_inclusions() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src/bin")).unwrap();
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("src/lib.rs"),
            "#[cfg(test)] mod tests { use super::*; }\nmod a;\n",
        )
        .unwrap();
        std::fs::write(root.path().join("src/a.rs"), "fn helper() {}\n").unwrap();
        assert!(scan_safe_private_import_environment(root.path()));
        std::fs::write(root.path().join("src/inc.rſ"), "include!(\"a.rs\");\n").unwrap();
        assert!(!scan_safe_private_import_environment(root.path()));
        std::fs::remove_file(root.path().join("src/inc.rſ")).unwrap();
        std::fs::create_dir_all(root.path().join("other")).unwrap();
        std::fs::write(
            root.path().join("other/cargo.toml"),
            "[lib]\npath = \"a.rs\"\n",
        )
        .unwrap();
        assert!(!scan_safe_private_import_environment(root.path()));
        std::fs::remove_file(root.path().join("other/cargo.toml")).unwrap();
        assert!(!scan_safe_private_import_environment(
            &root.path().join("src")
        ));

        std::fs::write(
            root.path().join("src/bin/tool.rs"),
            "#[path = \"../a.rs\"] mod util;\nfn main() {}\n",
        )
        .unwrap();
        assert!(!scan_safe_private_import_environment(root.path()));
        std::fs::write(
            root.path().join("src/bin/tool.rs"),
            "include!(\"../a.rs\");\n",
        )
        .unwrap();
        assert!(!scan_safe_private_import_environment(root.path()));
        std::fs::write(
            root.path().join("src/bin/tool.rs"),
            "std /*a*/ :: /*b*/ include!{\"../a.rs\"}\n",
        )
        .unwrap();
        assert!(!scan_safe_private_import_environment(root.path()));
        std::fs::remove_file(root.path().join("src/bin/tool.rs")).unwrap();
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\npath = \"src/a.rs\"\n",
        )
        .unwrap();
        assert!(!scan_safe_private_import_environment(root.path()));
    }

    #[test]
    fn private_widening_ignores_nonpublic_parent_globs() {
        for location in ["src/lib.rs", "src/numeric.rs", "src/unrelated.rs"] {
            let root = tempfile::tempdir().unwrap();
            std::fs::create_dir(root.path().join("src")).unwrap();
            for (path, source) in [
                (
                    "Cargo.toml",
                    "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
                ),
                (
                    "src/lib.rs",
                    "mod numeric;\nmod unrelated;\npub use std::fmt::Debug;\n",
                ),
                (
                    "src/numeric.rs",
                    "fn parse() {}\npub fn run() { parse(); }\n",
                ),
                ("src/unrelated.rs", "pub fn other() {}\n"),
            ] {
                let suffix = if path == location {
                    "#[cfg(test)] mod tests { use super::*; #[test] fn works() { assert!(true); } }\n"
                } else {
                    ""
                };
                std::fs::write(root.path().join(path), format!("{source}{suffix}")).unwrap();
            }
            let proof = RustPathOwnerProjectProof::new(root.path(), true, true, true, &[], "");
            assert!(proof.allows_private_widening_paths("parse"), "{location}");
        }
    }

    #[test]
    fn private_widening_parent_globs_keep_possible_name_transport_blocking() {
        for extra in [
            "use crate::numeric::*;",
            "#[cfg(test)] mod tests { pub use super::*; }",
            "use crate::numeric::parse as renamed; #[cfg(test)] mod tests { use super::*; }",
            "pub use crate::numeric::parse as renamed; #[cfg(test)] mod tests { use super::*; }",
            "macro_rules! assert { ($e:expr) => {} } #[cfg(test)] mod tests { use super::*; #[test] fn works() { assert!(true); } }",
            "macro_rules! r#assert { ($e:expr) => {} } #[cfg(test)] mod tests { use super::*; #[test] fn works() { assert!(true); } }",
        ] {
            let root = tempfile::tempdir().unwrap();
            std::fs::create_dir(root.path().join("src")).unwrap();
            std::fs::write(
                root.path().join("Cargo.toml"),
                "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            )
            .unwrap();
            std::fs::write(
                root.path().join("src/lib.rs"),
                "mod numeric;\nmod unrelated;\n",
            )
            .unwrap();
            std::fs::write(
                root.path().join("src/numeric.rs"),
                "pub(crate) fn parse() {}\n",
            )
            .unwrap();
            std::fs::write(root.path().join("src/unrelated.rs"), extra).unwrap();
            let proof = RustPathOwnerProjectProof::new(root.path(), true, true, true, &[], "");
            assert!(!proof.allows_private_widening_paths("parse"), "{extra}");
            if extra.starts_with("use crate::numeric::parse as renamed;") {
                assert!(proof.allows_private_widening_paths("other"), "{extra}");
                assert!(!proof.allows_private_widening_paths("parse"), "{extra}");
            }
        }
    }

    /// 可視性だけの拡大の分類は、宣言と参照が同じ crate root の通常のモジュールに属すると
    /// 証明できたときだけ行う。別の crate (bin) の参照と、所属を決められない参照 (tests/)
    /// は分類しない (`None`)。
    #[test]
    fn explicit_widening_scope_needs_one_crate_and_a_proven_module() {
        use super::super::rust_visibility_scope::{WideningScope, explicit_widenings};
        let root = tempfile::tempdir().unwrap();
        for directory in ["src/numeric", "tests"] {
            std::fs::create_dir_all(root.path().join(directory)).unwrap();
        }
        for (path, content) in [
            (
                "Cargo.toml",
                "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("src/lib.rs", "mod numeric;\nmod unrelated;\n"),
            ("src/main.rs", "mod tool;\nfn main() {}\n"),
            ("src/tool.rs", "pub fn run() {}\n"),
            ("src/numeric/mod.rs", "mod ascii;\nmod sibling;\n"),
            ("src/numeric/ascii.rs", "pub(super) fn parse() {}\n"),
            ("src/numeric/sibling.rs", "pub fn run() {}\n"),
            ("src/unrelated.rs", "pub fn run() {}\n"),
            ("tests/flow.rs", "fn run() {}\n"),
        ] {
            std::fs::write(root.path().join(path), content).unwrap();
        }
        let new = "pub(super) fn parse() {}\n";
        let patch = format!(
            "--- a/src/numeric/ascii.rs\n+++ b/src/numeric/ascii.rs\n@@ -1 +1 @@\n-fn parse() {{}}\n+{new}"
        );
        let tree = parser::parse_source(new.as_bytes(), LangId::Rust).unwrap();
        let changed = [crate::models::impact::AffectedSymbol {
            change_evidence: None,
            name: "parse".into(),
            kind: "function".into(),
            change_type: "modified".into(),
        }];
        let proofs = explicit_widenings(
            &patch,
            "src/numeric/ascii.rs",
            new.as_bytes(),
            tree.root_node(),
            &changed,
        );
        let scope = |project: &RustPathOwnerProjectProof, reference: &str| {
            project.explicit_widening_scope(
                "src/numeric/ascii.rs",
                reference,
                "parse",
                &proofs["parse"],
            )
        };
        let project = RustPathOwnerProjectProof::new(root.path(), true, true, true, &[], "");
        assert_eq!(
            scope(&project, "src/unrelated.rs"),
            Some(WideningScope::OutsideNew)
        );
        assert_eq!(
            scope(&project, "src/numeric/sibling.rs"),
            Some(WideningScope::NewlyVisible)
        );
        assert_eq!(scope(&project, "src/tool.rs"), None);
        assert_eq!(scope(&project, "tests/flow.rs"), None);
        // 非公開の親 glob は、ascii の中の parse を新たに取り込まない。
        std::fs::write(
            root.path().join("src/numeric/sibling.rs"),
            "use super::*;\npub fn run() {}\n",
        )
        .unwrap();
        let project = RustPathOwnerProjectProof::new(root.path(), true, true, true, &[], "");
        assert_eq!(
            scope(&project, "src/unrelated.rs"),
            Some(WideningScope::OutsideNew)
        );
        std::fs::write(
            root.path().join("src/numeric/sibling.rs"),
            "use super::ascii::*;\npub fn run() {}\n",
        )
        .unwrap();
        let project = RustPathOwnerProjectProof::new(root.path(), true, true, true, &[], "");
        assert_eq!(scope(&project, "src/unrelated.rs"), None);
    }

    #[test]
    fn direct_import_index_requires_an_unconditional_unique_binding() {
        for source in [
            "#[cfg(test)] use crate::util::helper;\nfn call() { helper(); }\n",
            "use crate::util::helper;\nuse super::helper;\nfn call() { helper(); }\n",
            "use crate::util::helper;\nuse super::*;\nfn call() { helper(); }\n",
        ] {
            let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
            let index = RustPathOwnerIndex::build(tree.root_node(), source.as_bytes());
            assert!(!index.direct_imports.contains_key("helper"), "{source}");
        }
        let source = b"use crate::util::helper;\nfn call() { helper(); }\n";
        let tree = parser::parse_source(source, LangId::Rust).unwrap();
        let index = RustPathOwnerIndex::build(tree.root_node(), source);
        assert!(index.direct_imports.contains_key("helper"));
        let source = b"type Alias = super::Alias;\nstruct Local;\n";
        let tree = parser::parse_source(source, LangId::Rust).unwrap();
        let index = RustPathOwnerIndex::build(tree.root_node(), source);
        assert!(!index.local_type_names.contains("Alias"));
        assert!(index.local_type_names.contains("Local"));
        assert!(!source_may_include_elsewhere(
            b"#[cfg(test)] mod tests { use super::*; }\n"
        ));
        assert!(!source_may_include_elsewhere(
            b"fn helper(value: u8) -> u8 { value }\n#[cfg(test)] mod tests { use super::*; #[test] fn works() { assert_eq!(helper(1), 1); } }\n"
        ));
    }

    #[test]
    fn direct_local_function_owns_plain_calls_despite_body_macros() {
        let source = b"fn helper() -> u8 { println!(\"x\"); 1 }\nfn call() -> u8 { helper() }\n";
        let tree = parser::parse_source(source, LangId::Rust).unwrap();
        let index = RustPathOwnerIndex::build(tree.root_node(), source);
        let root = tree.root_node();
        let mut pending = vec![root];
        let mut call = None;
        while let Some(node) = pending.pop() {
            if node.kind() == "identifier"
                && node.utf8_text(source).ok() == Some("helper")
                && node.start_position().row == 1
            {
                call = Some(node);
                break;
            }
            let mut cursor = node.walk();
            pending.extend(node.named_children(&mut cursor));
        }
        assert!(index.plain_local_value_owner(call.unwrap(), source, "helper"));
    }

    #[test]
    fn local_value_owner_distinguishes_patterns_and_nested_scopes() {
        for query in [
            "(function_signature_item)",
            "(shorthand_field_identifier)",
            "(for_expression pattern: (_))",
            "(let_condition pattern: (_))",
        ] {
            tree_sitter::Query::new(&LangId::Rust.ts_language(), query).unwrap();
        }
        for (parameters, body, expected) in [
            ("", "let value = record.normalize; normalize(1)", true),
            ("", "let normalize = other; normalize(1)", false),
            ("normalize: fn(u8) -> u8", "normalize(1)", false),
            (
                "",
                "if let Some(normalize) = value { normalize(1) } else { 0 }",
                false,
            ),
            ("", "for normalize in values { normalize(1); } 0", false),
            (
                "",
                "match value { Some(normalize) => normalize(1), _ => 0 }",
                false,
            ),
            ("", "let Binding { normalize } = value; normalize(1)", false),
            ("", "let pattern!() = value; normalize(1)", false),
            ("", "unknown!(); normalize(1)", false),
            ("", "{ unknown!(); } normalize(1)", true),
            ("", "let value = unknown!(); normalize(1)", true),
            ("", "{ let normalize = other; } normalize(1)", true),
            ("", "fn inner() { unknown!(); } normalize(1)", true),
            (
                "",
                "unsafe extern \"C\" { fn normalize(x: u8) -> u8; } normalize(1)",
                false,
            ),
        ] {
            let source = format!(
                "use crate::other::normalize_signature_range;\nfn normalize(x: u8) -> u8 {{ x }}\nfn local({parameters}) -> u8 {{ {body} }}\n"
            );
            let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
            assert!(!tree.root_node().has_error(), "{source}");
            let index = RustPathOwnerIndex::build(tree.root_node(), source.as_bytes());
            let at = source.rfind("normalize(1)").unwrap();
            let node = tree
                .root_node()
                .descendant_for_byte_range(at, at + "normalize".len())
                .unwrap();
            assert_eq!(
                index.plain_local_value_owner(node, source.as_bytes(), "normalize"),
                expected,
                "{source}"
            );
        }
    }

    #[test]
    fn local_value_owner_does_not_bind_a_call_in_a_let_initializer() {
        let source = b"fn normalize(x: u8) -> u8 { x }\npub fn local() -> u8 { let result = normalize(1); if matches!(result, 0) { 0 } else { result } }\n#[cfg(test)] mod tests {}\n";
        let tree = parser::parse_source(source, LangId::Rust).unwrap();
        let root = tree.root_node();
        let index = RustPathOwnerIndex::build(root, source);
        let _ = crate::engine::tree_walk::walk_preorder(
            root,
            crate::engine::tree_walk::Children::Named,
            |node, _| {
                if node.kind() == "identifier"
                    && node.utf8_text(source).ok() == Some("normalize")
                    && node.start_position().row == 1
                {
                    assert!(index.local_value_names.contains("normalize"));
                    assert!(index.plain_local_value_owner(node, source, "normalize"));
                }
                std::ops::ControlFlow::<(), _>::Continue(crate::engine::tree_walk::Walk::Descend)
            },
        );
    }

    #[test]
    fn private_leaf_proof_keeps_external_child_and_inclusion_barriers() {
        assert!(!source_has_external_child_module(
            b"fn helper() {}\n#[cfg(test)] mod tests { use super::*; #[test] fn works() { assert_eq!(1, 1); } }\n"
        ));
        assert!(source_has_external_child_module(
            b"fn helper() {}\nmod child;\n"
        ));
        assert!(source_has_external_child_module(
            b"fn helper() {}\nmake_child!();\n"
        ));
        assert!(source_has_include_macro(b"include!(\"a.rs\");\n"));
        assert!(source_has_include_macro(
            b"cfg_if::cfg_if! { if #[cfg(unix)] { include!(\"a.rs\"); } }\n"
        ));
        assert!(source_has_include_macro(
            b"macro_rules! inc { () => { include!(\"a.rs\"); } }\ninc!();\n"
        ));
        assert!(source_has_include_macro(b"use std::include as load;\n"));
        assert!(source_has_include_macro(b"std :: include!(\"a.rs\");\n"));
        assert!(source_has_include_macro(
            b"std /*a*/ :: /*b*/ include!{\"a.rs\"}\n"
        ));
        assert!(source_has_include_macro(b"include /*c*/ ! [\"a.rs\"];\n"));
        assert!(source_has_include_macro(b"r#include!(\"a.rs\");\n"));
        assert!(source_has_include_macro(
            b"macro_rules! inc { () => { r#include!(\"a.rs\"); } }\n"
        ));
        assert!(!source_has_include_macro(
            b"std :: include_str!(\"a.rs\");\n"
        ));
        assert!(!source_has_include_macro(b"my::include_all!(\"a.rs\");\n"));
        assert!(!source_has_include_macro(
            b"fn f() { println!(\"{}\", args.include); }\n"
        ));
        assert!(source_has_include_macro(
            b"macro_rules! m { () => { x . include!(\"a.rs\"); } }\n"
        ));
        assert!(!source_has_include_macro(
            b"fn helper() { println!(\"include! in text\"); }\n"
        ));
    }

    #[test]
    fn private_leaf_include_imports_match_the_identifier() {
        for source in [
            "use paths::compile_includes;\n",
            "use paths::{compile_includes, compile_excludes};\n",
            "use paths::include_helper as compile_includes;\n",
            "use std::{include_str, include_bytes};\n",
            "use paths /* include */ :: compile_includes;\n",
            "use paths::{r#compile_includes};\n",
            "use paths::includes;\n",
        ] {
            assert!(!source_has_include_macro(source.as_bytes()), "{source}");
        }
        for source in [
            "use std::include as load;\n",
            "use std::{include as load};\n",
            "use std /* note */ :: r#include as load;\n",
            "use std::{r#include as load, include_str};\n",
            "use custom::load as include;\n",
            "use std::{self, nested::{r#include as load}};\n",
            "fn f() { assert!(include.matches()); }\n",
            "macro_rules! load { ($head:ident.$tail:ident) => { $head!(\"a.rs\"); } }\nload!(include.any);\n",
        ] {
            assert!(source_has_include_macro(source.as_bytes()), "{source}");
        }
        assert!(source_has_include_macro(
            b"use paths /* include \xff */ ::compile_includes;\n"
        ));
        assert!(source_has_include_macro(b"use std::{include;\n"));
    }

    #[test]
    fn private_leaf_proof_rejects_commented_include_in_another_source() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(root.path().join("src/lib.rs"), "mod a; mod b;\n").unwrap();
        std::fs::write(root.path().join("src/a.rs"), "fn helper() {}\n").unwrap();
        std::fs::write(root.path().join("src/b.rs"), "fn call() {}\n").unwrap();
        let plain = RustPathOwnerProjectProof::new(root.path(), true, true, true, &[], "");
        assert!(plain.private_source_is_leaf("src/a.rs"));

        std::fs::write(
            root.path().join("src/b.rs"),
            "std /*a*/ :: /*b*/ include!{\"a.rs\"}\n",
        )
        .unwrap();
        let included = RustPathOwnerProjectProof::new(root.path(), true, true, true, &[], "");
        assert!(!included.private_source_is_leaf("src/a.rs"));
    }

    #[test]
    fn source_scan_rejects_case_folded_extensions_and_skipped_target_members() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        for extension in ["RS", "rſ"] {
            let path = root.path().join("src").join(format!("inc.{extension}"));
            std::fs::write(&path, "#[path = \"other.rs\"] mod child;\n").unwrap();
            assert!(!scan_ordinary_sources(root.path()), "{extension}");
            std::fs::remove_file(path).unwrap();
        }
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"TARGET/member\"]\n",
        )
        .unwrap();
        assert!(!scan_ordinary_sources(root.path()));
    }

    #[test]
    fn source_scan_rejects_other_cargo_roots_and_implicit_build_targets() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        assert!(scan_ordinary_sources(root.path()));

        std::fs::create_dir(root.path().join("nested")).unwrap();
        std::fs::write(root.path().join("nested/Cargo.toml"), "[package]\n").unwrap();
        assert!(!scan_ordinary_sources(root.path()));
        std::fs::remove_file(root.path().join("nested/Cargo.toml")).unwrap();

        std::fs::write(root.path().join("build.rs"), "fn main() {}\n").unwrap();
        assert!(!scan_ordinary_sources(root.path()));
        std::fs::remove_file(root.path().join("build.rs")).unwrap();

        std::fs::write(root.path().join("src/mod.rs"), "mod other;\n").unwrap();
        assert!(!scan_ordinary_sources(root.path()));
        std::fs::remove_file(root.path().join("src/mod.rs")).unwrap();

        std::fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\n[dependencies]\nother = { path = \"target/other\" }\n",
        )
        .unwrap();
        assert!(!scan_ordinary_sources(root.path()));

        std::fs::create_dir(root.path().join("child")).unwrap();
        std::fs::write(
            root.path().join("child/Cargo.toml"),
            "[package]\nname = \"child\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        assert!(!scan_ordinary_sources(&root.path().join("child")));
    }

    /// 修正前の再帰版 (浅い入力で反復版と比べるためだけに残す)。
    fn path_segments_recursive(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<Vec<String>> {
        match node.kind() {
            "identifier" | "type_identifier" | "crate" | "self" | "super" => {
                Some(vec![node.utf8_text(source).ok()?.to_owned()])
            }
            "scoped_identifier" | "scoped_type_identifier" => {
                let mut segments =
                    path_segments_recursive(node.child_by_field_name("path")?, source)?;
                segments.push(
                    node.child_by_field_name("name")?
                        .utf8_text(source)
                        .ok()?
                        .to_owned(),
                );
                Some(segments)
            }
            _ => None,
        }
    }

    /// 反復に書き換えた path の分解が、浅い入力の全ノードで修正前の再帰版と同じ結果を返す
    /// (`path` の無い `::name` や式・ジェネリクスのように分解できない形を含む)。
    #[test]
    fn iterative_path_segments_match_recursive_version() {
        let source = b"use crate::a::b::{c, d::e};\nfn f(x: self::G<super::H>, y: ::std::ops::Range<u8>, z: <T as Tr>::C) -> crate::m::N { super::super::g::h(x); a::b::<u8>::c(); }\n";
        let tree = parser::parse_source(source, LangId::Rust).unwrap();
        let mut cursor = tree.root_node().walk();
        let mut nodes = vec![tree.root_node()];
        let mut next = 0;
        let mut resolved = 0;
        while next < nodes.len() {
            let node = nodes[next];
            nodes.extend(node.children(&mut cursor));
            next += 1;
            let iterative = super::path_segments(node, source);
            assert_eq!(
                iterative,
                path_segments_recursive(node, source),
                "{}: {}",
                node.kind(),
                node.utf8_text(source).unwrap()
            );
            resolved += usize::from(iterative.is_some_and(|segments| segments.len() > 1));
        }
        assert!(resolved >= 8, "修飾パスを含む: {resolved}");
    }

    /// `include!` を許す配置と許さない配置は、どちらを先に計算しても、それぞれを単独で走査した
    /// 結果と同じになる (Issue #470)。
    #[test]
    fn static_layouts_agree_in_either_order() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(root.path().join("src/a.rs"), "pub fn helper() {}\n").unwrap();
        // (lib.rs, 許さない側が Some か, 許す側が Some か)
        for (lib, plain_some, literal_some) in [
            ("#[path = \"a.rs\"]\nmod moved;\n", true, true),
            ("mod copy {\n    include!(\"a.rs\");\n}\n", false, true),
            ("include!(concat!(\"a\", \".rs\"));\n", false, false),
        ] {
            std::fs::write(root.path().join("src/lib.rs"), lib).unwrap();
            let fresh =
                || RustPathOwnerProjectProof::new(root.path(), false, false, false, &[], "");
            let plain = super::enum_owner::static_layout(root.path(), &fresh(), false)
                .map(|(relocated, _, _)| relocated);
            let literal = super::enum_owner::static_layout(root.path(), &fresh(), true)
                .map(|(relocated, _, _)| relocated);
            assert_eq!(
                (plain.is_some(), literal.is_some()),
                (plain_some, literal_some),
                "{lib}"
            );
            if plain_some {
                assert!(plain.as_ref().is_some_and(|list| !list.is_empty()), "{lib}");
            }
            for plain_first in [true, false] {
                let project = fresh();
                let (first, second) = if plain_first {
                    (project.static_layout(false), project.static_layout(true))
                } else {
                    (project.static_layout(true), project.static_layout(false))
                };
                let (got_plain, got_literal) = if plain_first {
                    (first, second)
                } else {
                    (second, first)
                };
                assert_eq!(got_plain, plain.as_ref(), "{lib} plain_first={plain_first}");
                assert_eq!(
                    got_literal,
                    literal.as_ref(),
                    "{lib} plain_first={plain_first}"
                );
            }
        }
    }

    /// `#[path]` で読んだファイルが子の `mod` を宣言し得るなら、そのディレクトリごと配置を保留する
    /// (mod.rs と同じく、子を自分のディレクトリから探すため)。子の無いファイルはファイルだけ。
    #[test]
    fn static_layout_holds_the_directory_of_a_path_target_with_child_modules() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        std::fs::create_dir_all(root.path().join("shared")).unwrap();
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("src/lib.rs"),
            "#[path = \"../shared/bridge.rs\"]\nmod bridge;\n",
        )
        .unwrap();
        std::fs::write(root.path().join("shared/calls.rs"), "pub fn call() {}\n").unwrap();
        let canonical = root.path().canonicalize().unwrap();
        let shared = canonical.join("shared");
        for (bridge, holds_directory) in [
            ("mod calls;\n", true),
            (
                "macro_rules! child { () => { mod calls; }; }\nchild!();\n",
                true,
            ),
            ("pub fn bridge() { calls(); }\nfn calls() {}\n", false),
            (r#"fn bridge() { println!("mod calls;"); }"#, false),
        ] {
            std::fs::write(root.path().join("shared/bridge.rs"), bridge).unwrap();
            let project = RustPathOwnerProjectProof::new(root.path(), false, false, false, &[], "");
            let relocated = project.static_layout(true).unwrap().clone();
            assert_eq!(
                relocated.contains(&shared),
                holds_directory,
                "{bridge}: {relocated:?}"
            );
            assert!(
                relocated
                    .iter()
                    .any(|path| shared.join("bridge.rs").starts_with(path)),
                "{bridge}: {relocated:?}"
            );
            assert_eq!(
                project.unrelocated(std::path::Path::new("shared/calls.rs")),
                !holds_directory,
                "{bridge}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn relative_to_root_ignores_the_verbatim_prefix() {
        use std::path::{Path, PathBuf};
        for (root, path, expected) in [
            (
                r"\\?\C:\work\repo",
                r"C:\work\repo\target\probe",
                Some(PathBuf::from(r"target\probe")),
            ),
            (
                r"C:\work\repo",
                r"\\?\c:\WORK\repo\target",
                Some(PathBuf::from("target")),
            ),
            (
                r"\\?\UNC\server\share\repo",
                r"\\server\share\repo\target",
                Some(PathBuf::from("target")),
            ),
            (r"\\?\C:\work\repo", r"D:\work\repo\target", None),
        ] {
            assert_eq!(
                super::relative_to_root(Path::new(root), Path::new(path)),
                expected,
                "{root} {path}"
            );
        }
    }
}
