//! 削除判定の配置証拠。解析 root を Cargo の実行場所として入力集合を閉じる。
//!
//! 無視された独立 package、未展開 proc macro と解析範囲外のマクロ定義による配置変更は保証の範囲外。
//! メンバーなど別の実行場所だけで有効になる Cargo config も扱わない。

use super::{ChildModule, FileScopes, ModuleRef};
use crate::engine::{bounded_read::read_input_file, parser};
use crate::language::LangId;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

const MAX_FILES: usize = 10_000;
const MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;
const MAX_ENTRIES: usize = 100_000;
const MAX_ROOTS: usize = 2_048;
const MAX_OCCURRENCES: usize = 100_000;
const MAX_EDGES: usize = 100_000;
const MAX_MEMBERSHIPS: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum TargetKind {
    Library,
    BuildScript,
    Other,
}

pub(super) struct CrateInput {
    pub(super) package: Rc<str>,
    pub(super) kind: TargetKind,
}

struct Occurrence {
    module: ModuleRef,
    parent: Option<usize>,
    root: usize,
    directory: PathBuf,
    certain: bool,
    edition: bool,
    children: HashMap<String, Vec<usize>>,
}

pub(super) struct Layout {
    files: HashMap<Rc<str>, Rc<FileScopes>>,
    occurrences: Vec<Occurrence>,
    memberships: HashMap<Rc<str>, Vec<usize>>,
    scopes: HashMap<(usize, usize), usize>,
    unknown_directories: Vec<PathBuf>,
    unproven_files: HashSet<Rc<str>>,
    // 同じ実体を異なる Cargo target が使う場合も、root のコンパイル文脈は分けて保持する。
    pub(super) packages: HashMap<Rc<str>, Rc<toml::Value>>,
    /// 入力集合の監査で確認した表記から、manifest の実体が属する package への対応。
    package_paths: HashMap<Rc<str>, Rc<str>>,
    pub(super) crate_inputs: HashMap<usize, CrateInput>,
    pub(super) library_roots: HashMap<Rc<str>, usize>,
}

impl Layout {
    pub(super) fn build(dir: &Path) -> Option<Self> {
        Builder::new(dir)?.build()
    }

    pub(super) fn contexts(&self, file: &str) -> Option<&[usize]> {
        if self.unproven_files.contains(file) {
            return None;
        }
        let contexts = self.memberships.get(file)?;
        (!contexts.is_empty()).then_some(contexts)
    }

    #[cfg(test)]
    pub(super) fn audited_directory_count(&self) -> usize {
        self.unknown_directories.len()
    }

    pub(super) fn file_scopes(&self, file: &str) -> Option<Rc<FileScopes>> {
        self.files.get(file).cloned()
    }

    pub(super) fn scope_occurrence(&self, file_root: usize, scope: usize) -> Option<usize> {
        self.scopes.get(&(file_root, scope)).copied()
    }

    pub(super) fn crate_root(&self, occurrence: usize) -> ModuleRef {
        self.occurrences[self.occurrences[occurrence].root]
            .module
            .clone()
    }

    pub(super) fn package_at(&self, base: &str, raw: &str) -> Option<Rc<str>> {
        let path = normalized(&join_relative(Path::new(base), raw)?)?;
        let package = self.package_paths.get(path.as_str())?;
        self.packages
            .get_key_value(package.as_ref())
            .map(|(key, _)| key.clone())
    }

    pub(super) fn parent(&self, occurrence: usize) -> Option<ModuleRef> {
        let parent = self.occurrences.get(occurrence)?.parent?;
        Some(self.occurrences[parent].module.clone())
    }

    pub(super) fn child(&self, occurrence: usize, name: &str) -> Option<ModuleRef> {
        let children = self.occurrences.get(occurrence)?.children.get(name)?;
        let [index] = children.as_slice() else {
            return None;
        };
        let child = self.occurrences.get(*index)?;
        child.certain.then(|| child.module.clone())
    }
}

struct Builder {
    dir: PathBuf,
    logical_dir: PathBuf,
    layout: Layout,
    manifests: HashMap<PathBuf, Rc<toml::Value>>,
    directories: HashMap<PathBuf, Rc<Vec<PathBuf>>>,
    sources: HashSet<PathBuf>,
    bytes: usize,
    entries: usize,
    edges: usize,
}

impl Builder {
    fn new(dir: &Path) -> Option<Self> {
        let logical_dir: PathBuf = std::path::absolute(dir).ok()?.components().collect();
        let metadata = std::fs::symlink_metadata(&logical_dir).ok()?;
        if is_link(&metadata) || !metadata.is_dir() {
            return None;
        }
        // OS の祖先 alias は解消するが、root 自身や配下のリンクは許可しない。
        let dir = std::fs::canonicalize(&logical_dir).ok()?;
        Some(Self {
            dir,
            logical_dir,
            layout: Layout {
                files: HashMap::new(),
                occurrences: Vec::new(),
                memberships: HashMap::new(),
                scopes: HashMap::new(),
                unknown_directories: Vec::new(),
                unproven_files: HashSet::new(),
                packages: HashMap::new(),
                package_paths: HashMap::new(),
                crate_inputs: HashMap::new(),
                library_roots: HashMap::new(),
            },
            manifests: HashMap::new(),
            directories: HashMap::new(),
            sources: HashSet::new(),
            bytes: 0,
            entries: 0,
            edges: 0,
        })
    }

    fn build(mut self) -> Option<Layout> {
        self.check_configs()?;
        for ancestor in self
            .dir
            .ancestors()
            .skip(1)
            .chain(self.logical_dir.ancestors().skip(1))
        {
            match std::fs::symlink_metadata(ancestor.join("Cargo.toml")) {
                Ok(_) => return None,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return None,
            }
        }
        let mut queue = VecDeque::new();
        let mut walk = ignore::WalkBuilder::new(&self.dir);
        walk.hidden(false)
            .git_ignore(true)
            .git_global(true)
            .filter_entry(|entry| entry.file_name() != ".git");
        for entry in walk.build() {
            let entry = entry.ok()?;
            self.spend_entry()?;
            if entry.file_type()?.is_symlink() {
                return None;
            }
            if entry.file_type()?.is_file()
                && entry
                    .file_name()
                    .to_str()?
                    .eq_ignore_ascii_case("Cargo.toml")
            {
                queue.push_back(entry.path().strip_prefix(&self.dir).ok()?.to_owned());
            }
        }
        let mut closed = HashSet::new();
        let mut input_edges = queue.len();
        while let Some(raw_path) = queue.pop_front() {
            let path = self.plain(&raw_path, false)??;
            let package: Rc<str> = normalized(path.parent()?)?.into();
            // 同じ manifest を既に読んでいても、依存の表記違いは先に記録する。
            for alias in [normalized(raw_path.parent()?)?.into(), package.clone()] {
                if let Some(previous) = self.layout.package_paths.insert(alias, package.clone())
                    && previous != package
                {
                    return None;
                }
            }
            if self.layout.package_paths.len() > MAX_ENTRIES {
                return None;
            }
            if !closed.insert(path.clone()) {
                continue;
            }
            let manifest = self.manifest(&path)?;
            let base = path.parent()?;
            let queued_before = queue.len();
            if let Some(workspace) = manifest.get("workspace") {
                let workspace = workspace.as_table()?;
                if let Some(members) = workspace.get("members") {
                    for member in members.as_array()? {
                        queue.extend(self.expand_member(base, member.as_str()?)?);
                    }
                }
            }
            if let Some(package) = manifest.get("package").or_else(|| manifest.get("project")) {
                let package = package.as_table()?;
                if let Some(workspace) = package.get("workspace") {
                    queue.push_back(join_relative(base, workspace.as_str()?)?.join("Cargo.toml"));
                } else if let Some(workspace) =
                    self.ancestor_workspace(base, manifest.get("workspace").is_some())?
                {
                    queue.push_back(workspace);
                }
            }
            collect_path_inputs(&manifest, base, &mut queue)?;
            input_edges = input_edges.checked_add(queue.len().checked_sub(queued_before)?)?;
            if input_edges > MAX_EDGES {
                return None;
            }
        }
        // 入力集合を閉じてから root を確定する。後から入力を足して肯定結果を変えない。
        let mut manifests: Vec<_> = closed.into_iter().collect();
        manifests.sort();
        let mut roots = Vec::new();
        for path in manifests {
            let manifest = self.manifest(&path)?;
            let package: Rc<str> = normalized(path.parent()?)?.into();
            self.layout
                .packages
                .insert(package.clone(), manifest.clone());
            if manifest
                .get("package")
                .or_else(|| manifest.get("project"))
                .is_none()
            {
                continue;
            }
            let edition = self.edition(&path, &manifest)?;
            for (root, kind) in self.roots(&path, &manifest)? {
                roots.push((root, edition, package.clone(), kind));
                if roots.len() > MAX_ROOTS {
                    return None;
                }
            }
        }
        for (root, edition, package, kind) in roots {
            let root = self.plain(&root, false)??;
            let directory = root.parent()?.to_owned();
            let index = self.add_occurrence(&root, 0, None, 0, directory, true, edition, None)?;
            self.layout.occurrences[index].root = index;
            if kind == TargetKind::Library {
                self.layout.library_roots.insert(package.clone(), index);
            }
            self.layout
                .crate_inputs
                .insert(index, CrateInput { package, kind });
            self.expand_graph(index, index)?;
        }
        self.audit_unknown_directories()?;
        self.index_membership_status();
        Some(self.layout)
    }

    fn spend_entry(&mut self) -> Option<()> {
        self.entries = self.entries.checked_add(1)?;
        (self.entries <= MAX_ENTRIES).then_some(())
    }

    fn read(&mut self, path: &Path) -> Option<Vec<u8>> {
        let length =
            usize::try_from(std::fs::symlink_metadata(self.dir.join(path)).ok()?.len()).ok()?;
        if length > MAX_FILE_BYTES || self.bytes.checked_add(length)? > MAX_BYTES {
            return None;
        }
        let source = read_input_file(&self.dir.join(path))?;
        self.bytes = self.bytes.checked_add(source.len())?;
        (source.len() <= MAX_FILE_BYTES && self.bytes <= MAX_BYTES).then_some(source)
    }

    /// NotFound だけが不在。symlink・junction・権限エラーは失敗として伝える。
    fn plain(&self, path: &Path, directory: bool) -> Option<Option<PathBuf>> {
        let mut full = self.dir.clone();
        for component in path.components() {
            let Component::Normal(component) = component else {
                return None;
            };
            full.push(component);
            match std::fs::symlink_metadata(&full) {
                Ok(metadata) if is_link(&metadata) => return None,
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(None),
                Err(_) => return None,
            }
        }
        let metadata = std::fs::symlink_metadata(&full).ok()?;
        if !(if directory {
            metadata.is_dir()
        } else {
            metadata.is_file()
        }) {
            return None;
        }
        let canonical = std::fs::canonicalize(full).ok()?;
        let relative = canonical.strip_prefix(&self.dir).ok()?.to_owned();
        relative.to_str()?;
        Some(Some(relative))
    }

    fn entries(&mut self, directory: &Path) -> Option<Rc<Vec<PathBuf>>> {
        if let Some(entries) = self.directories.get(directory) {
            return Some(entries.clone());
        }
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(self.dir.join(directory)).ok()? {
            let entry = entry.ok()?;
            self.spend_entry()?;
            let metadata = entry.metadata().ok()?;
            if entry.file_type().ok()?.is_symlink() || is_link(&metadata) {
                return None;
            }
            let path = entry.path().strip_prefix(&self.dir).ok()?.to_owned();
            path.to_str()?;
            entries.push(path);
        }
        entries.sort();
        let entries = Rc::new(entries);
        self.directories
            .insert(directory.to_owned(), entries.clone());
        Some(entries)
    }

    fn manifest(&mut self, path: &Path) -> Option<Rc<toml::Value>> {
        if let Some(manifest) = self.manifests.get(path) {
            return Some(manifest.clone());
        }
        if self.manifests.len() >= MAX_FILES {
            return None;
        }
        self.plain(path, false)??;
        let source = self.read(path)?;
        let value: toml::Value = toml::from_str(std::str::from_utf8(&source).ok()?).ok()?;
        value.as_table()?;
        let value = Rc::new(value);
        self.manifests.insert(path.to_owned(), value.clone());
        Some(value)
    }

    fn ancestor_workspace(&mut self, base: &Path, owns_workspace: bool) -> Option<Option<PathBuf>> {
        if owns_workspace {
            return Some(Some(base.join("Cargo.toml")));
        }
        let mut parent = base.parent();
        while let Some(directory) = parent {
            let path = directory.join("Cargo.toml");
            if let Some(path) = self.plain(&path, false)?
                && self.manifest(&path)?.get("workspace").is_some()
            {
                return Some(Some(path));
            }
            parent = directory.parent();
        }
        // 解析 root 外の workspace 継承や package からの別配置は閉じられない。
        for ancestor in self.dir.ancestors().skip(1) {
            match std::fs::symlink_metadata(ancestor.join("Cargo.toml")) {
                Ok(_) => return None,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return None,
            }
        }
        Some(None)
    }

    fn edition(&mut self, path: &Path, manifest: &toml::Value) -> Option<bool> {
        let package = manifest
            .get("package")
            .or_else(|| manifest.get("project"))?
            .as_table()?;
        let Some(edition) = package.get("edition") else {
            return Some(false);
        };
        let edition = if let Some(edition) = edition.as_str() {
            edition.to_owned()
        } else {
            if !edition.get("workspace")?.as_bool()? {
                return None;
            }
            let base = path.parent()?;
            let workspace = if let Some(workspace) = package.get("workspace") {
                join_relative(base, workspace.as_str()?)?.join("Cargo.toml")
            } else {
                self.ancestor_workspace(base, manifest.get("workspace").is_some())??
            };
            self.manifest(&workspace)?
                .get("workspace")?
                .get("package")?
                .get("edition")?
                .as_str()?
                .to_owned()
        };
        Some(matches!(edition.as_str(), "2018" | "2021" | "2024"))
    }

    fn expand_member(&mut self, base: &Path, pattern: &str) -> Option<Vec<PathBuf>> {
        if pattern.is_empty()
            || pattern.contains("**")
            || pattern.contains(['?', '[', ']', '{', '}', '\\'])
        {
            return None;
        }
        let pattern = join_relative(base, pattern)?;
        let mut directories = vec![PathBuf::new()];
        for component in pattern.components() {
            let Component::Normal(component) = component else {
                return None;
            };
            let component = component.to_str()?;
            let mut next = Vec::new();
            for directory in directories {
                if component.contains('*') {
                    let matcher = globset::Glob::new(component).ok()?.compile_matcher();
                    let directory = self.plain(&directory, true)??;
                    for entry in self.entries(&directory)?.iter() {
                        if matcher.is_match(entry.file_name()?)
                            && std::fs::symlink_metadata(self.dir.join(entry))
                                .ok()?
                                .is_dir()
                        {
                            next.push(entry.clone());
                        }
                    }
                } else {
                    next.push(directory.join(component));
                }
            }
            directories = next;
        }
        Some(
            directories
                .into_iter()
                .map(|directory| directory.join("Cargo.toml"))
                .collect(),
        )
    }

    fn roots(&mut self, path: &Path, manifest: &toml::Value) -> Option<Vec<(PathBuf, TargetKind)>> {
        let base = path.parent()?;
        let package = manifest
            .get("package")
            .or_else(|| manifest.get("project"))?
            .as_table()?;
        let mut roots = Vec::new();
        let explicit_lib = match manifest.get("lib") {
            Some(lib) => Some(lib.as_table()?),
            None => None,
        };
        if let Some(path) = explicit_lib.and_then(|lib| lib.get("path")) {
            roots.push((join_relative(base, path.as_str()?)?, TargetKind::Library));
        } else if let Some(path) = self.plain(&base.join("src/lib.rs"), false)? {
            roots.push((path, TargetKind::Library));
        }
        if let Some(path) = self.plain(&base.join("src/main.rs"), false)? {
            roots.push((path, TargetKind::Other));
        }
        for (kind, directory) in [
            ("bin", "src/bin"),
            ("example", "examples"),
            ("test", "tests"),
            ("bench", "benches"),
        ] {
            if let Some(targets) = manifest.get(kind) {
                for target in targets.as_array()? {
                    let target = target.as_table()?;
                    if let Some(path) = target.get("path") {
                        roots.push((join_relative(base, path.as_str()?)?, TargetKind::Other));
                    } else {
                        // 名前だけの target は自動検出の候補に含まれる必要がある。
                        let name = target.get("name")?.as_str()?;
                        if name.contains(['/', '\\']) || name.is_empty() {
                            return None;
                        }
                        let directory = base.join(directory);
                        let candidates = [
                            directory.join(format!("{name}.rs")),
                            directory.join(name).join("main.rs"),
                        ];
                        let mut found = false;
                        for candidate in candidates {
                            if let Some(path) = self.plain(&candidate, false)? {
                                roots.push((path, TargetKind::Other));
                                found = true;
                            }
                        }
                        if !found && kind != "bin" {
                            return None;
                        }
                        if !found && self.plain(&base.join("src/main.rs"), false)?.is_none() {
                            return None;
                        }
                    }
                }
            }
            let Some(directory) = self.plain(&base.join(directory), true)? else {
                continue;
            };
            for entry in self.entries(&directory)?.iter() {
                let full = self.dir.join(entry);
                let metadata = std::fs::symlink_metadata(&full).ok()?;
                if metadata.is_file()
                    && entry.extension().is_some_and(|extension| extension == "rs")
                {
                    roots.push((entry.clone(), TargetKind::Other));
                } else if metadata.is_dir()
                    && let Some(path) = self.plain(&entry.join("main.rs"), false)?
                {
                    roots.push((path, TargetKind::Other));
                }
            }
        }
        match package.get("build") {
            Some(toml::Value::Boolean(false)) => {}
            Some(toml::Value::String(path)) => {
                roots.push((join_relative(base, path)?, TargetKind::BuildScript))
            }
            None | Some(toml::Value::Boolean(true)) => {
                if let Some(path) = self.plain(&base.join("build.rs"), false)? {
                    roots.push((path, TargetKind::BuildScript));
                }
            }
            _ => return None,
        }
        roots.sort();
        roots.dedup();
        Some(roots)
    }

    fn load_source(&mut self, path: &Path) -> Option<Rc<str>> {
        let path = self.plain(path, false)??;
        let key: Rc<str> = normalized(&path)?.into();
        if self.sources.contains(&path) {
            return Some(key);
        }
        if self.sources.len() >= MAX_FILES {
            return None;
        }
        let source = self.read(&path)?;
        let tree = parser::parse_source(&source, LangId::Rust).ok()?;
        if !super::super::rust_ref_sites::clean_tree(tree.root_node())
            || crate::engine::rust_dynamic_source::tree_includes_external_source(
                tree.root_node(),
                &source,
            )
            || dynamic_macro_template(tree.root_node(), &source)
        {
            return None;
        }
        let scopes = Rc::new(FileScopes::collect(tree.root_node(), &source, None)?);
        self.layout.files.insert(key.clone(), scopes);
        self.sources.insert(path);
        Some(key)
    }

    #[allow(clippy::too_many_arguments)]
    fn add_occurrence(
        &mut self,
        path: &Path,
        scope: usize,
        parent: Option<usize>,
        root: usize,
        directory: PathBuf,
        certain: bool,
        edition: bool,
        file_root: Option<usize>,
    ) -> Option<usize> {
        if self.layout.occurrences.len() >= MAX_OCCURRENCES {
            return None;
        }
        let file = self.load_source(path)?;
        let index = self.layout.occurrences.len();
        if scope == 0 {
            let contexts = self.layout.memberships.entry(file.clone()).or_default();
            if contexts.len() >= MAX_MEMBERSHIPS {
                return None;
            }
            contexts.push(index);
        }
        self.layout
            .scopes
            .insert((file_root.unwrap_or(index), scope), index);
        self.layout.occurrences.push(Occurrence {
            module: ModuleRef {
                file,
                scope,
                occurrence: Some(index),
            },
            parent,
            root,
            directory,
            certain,
            edition,
            children: HashMap::new(),
        });
        Some(index)
    }

    fn expand_graph(&mut self, start: usize, file_root: usize) -> Option<()> {
        let mut pending = VecDeque::from([(start, file_root)]);
        while let Some((index, file_root)) = pending.pop_front() {
            let occurrence = &self.layout.occurrences[index];
            let module = occurrence.module.clone();
            let directory = occurrence.directory.clone();
            let (certain, edition, root) =
                (occurrence.certain, occurrence.edition, occurrence.root);
            let scopes = self.layout.files.get(&module.file)?.clone();
            let facts = scopes.scopes.get(module.scope)?;
            if facts.unknown_children {
                self.layout.unknown_directories.push(directory.clone());
            }
            for (name, child) in &facts.audit_modules {
                let reliable = certain && facts.child_module(name).is_some();
                let mut candidates = Vec::new();
                match child {
                    ChildModule::Inline(scope) => candidates.push((
                        PathBuf::from(module.file.as_ref()),
                        *scope,
                        directory.join(name),
                        Some(file_root),
                    )),
                    ChildModule::File => {
                        for candidate in [
                            directory.join(format!("{name}.rs")),
                            directory.join(name).join("mod.rs"),
                        ] {
                            if let Some(path) = self.plain(&candidate, false)? {
                                let child_dir = if candidate.file_name()?.to_str()? == "mod.rs" {
                                    path.parent()?.to_owned()
                                } else {
                                    path.with_extension("")
                                };
                                candidates.push((path, 0, child_dir, None));
                            }
                        }
                    }
                }
                let reliable = reliable && candidates.len() == 1;
                if candidates.is_empty() {
                    // 明示 mod の欠落も、正常な所属であるという証拠には使わない。
                    return None;
                }
                for (path, scope, child_dir, child_file_root) in candidates {
                    self.edges += 1;
                    if self.edges > MAX_EDGES {
                        return None;
                    }
                    let child_index = self.add_occurrence(
                        &path,
                        scope,
                        Some(index),
                        root,
                        child_dir,
                        reliable,
                        edition,
                        child_file_root,
                    )?;
                    self.layout.occurrences[index]
                        .children
                        .entry(name.clone())
                        .or_default()
                        .push(child_index);
                    pending.push_back((child_index, child_file_root.unwrap_or(child_index)));
                }
            }
        }
        Some(())
    }

    fn audit_unknown_directories(&mut self) -> Option<()> {
        let mut directories = Vec::new();
        for directory in &self.layout.unknown_directories {
            if let Some(directory) = self.plain(directory, true)? {
                directories.push(directory);
            }
        }
        directories.sort();
        directories.dedup();
        let mut roots: Vec<PathBuf> = Vec::new();
        for directory in directories {
            // 正規化して並べれば、直前の根だけで包含を判定できる。構成要素の境界は保つ。
            if roots.last().is_none_or(|root| !directory.starts_with(root)) {
                roots.push(directory);
            }
        }
        self.layout.unknown_directories = roots.clone();
        let mut pending = roots;
        let mut visited = HashSet::new();
        while let Some(directory) = pending.pop() {
            if !visited.insert(directory.clone()) {
                continue;
            }
            for entry in self.entries(&directory)?.iter() {
                let metadata = std::fs::symlink_metadata(self.dir.join(entry)).ok()?;
                if metadata.is_dir() {
                    pending.push(entry.clone());
                } else if metadata.is_file()
                    && entry
                        .extension()
                        .and_then(|extension| extension.to_str())
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("rs"))
                {
                    self.load_source(entry)?;
                }
            }
        }
        Some(())
    }

    fn index_membership_status(&mut self) {
        let directories: HashSet<_> = self
            .layout
            .unknown_directories
            .iter()
            .map(PathBuf::as_path)
            .collect();
        for (file, contexts) in &self.layout.memberships {
            let unknown = Path::new(file.as_ref())
                .ancestors()
                .any(|ancestor| directories.contains(ancestor));
            if unknown
                || contexts.iter().any(|&index| {
                    let occurrence = &self.layout.occurrences[index];
                    !occurrence.certain || !occurrence.edition
                })
            {
                self.layout.unproven_files.insert(file.clone());
            }
        }
    }

    fn check_configs(&mut self) -> Option<()> {
        let mut candidates = Vec::new();
        // 実体への正規化で、論理パス側の Cargo 入力を監査から落とさない。
        for directory in self.dir.ancestors().chain(self.logical_dir.ancestors()) {
            candidates.push(directory.join(".cargo/config"));
            candidates.push(directory.join(".cargo/config.toml"));
        }
        let cargo_home = std::env::var_os("CARGO_HOME")
            .map(|path| self.dir.join(path))
            .or_else(|| dirs::home_dir().map(|home| home.join(".cargo")));
        if let Some(cargo_home) = cargo_home {
            candidates.push(cargo_home.join("config"));
            candidates.push(cargo_home.join("config.toml"));
        }
        candidates.sort();
        candidates.dedup();
        for path in candidates {
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return None,
            };
            if is_link(&metadata) || !metadata.is_file() {
                return None;
            }
            if metadata.len() > MAX_FILE_BYTES as u64 {
                return None;
            }
            for ancestor in path.ancestors().skip(1) {
                if is_link(&std::fs::symlink_metadata(ancestor).ok()?)
                    && !self.logical_dir.starts_with(ancestor)
                    && !self.dir.starts_with(ancestor)
                {
                    return None;
                }
            }
            let source = read_input_file(&path)?;
            self.bytes = self.bytes.checked_add(source.len())?;
            if source.len() > MAX_FILE_BYTES || self.bytes > MAX_BYTES {
                return None;
            }
            let value: toml::Value = toml::from_str(std::str::from_utf8(&source).ok()?).ok()?;
            let table = value.as_table()?;
            if ["include", "patch", "paths", "replace", "source"]
                .iter()
                .any(|key| table.contains_key(*key))
            {
                return None;
            }
        }
        Some(())
    }
}

fn normalized(path: &Path) -> Option<String> {
    Some(crate::git_support::normalize_workspace_separators(
        path.to_str()?,
    ))
}

fn join_relative(base: &Path, raw: &str) -> Option<PathBuf> {
    if raw.is_empty() || Path::new(raw).is_absolute() {
        return None;
    }
    let mut path = base.to_owned();
    for component in Path::new(raw).components() {
        match component {
            Component::Normal(component) => path.push(component),
            Component::CurDir => {}
            Component::ParentDir if path.pop() => {}
            _ => return None,
        }
    }
    Some(path)
}

fn collect_path_inputs(
    value: &toml::Value,
    base: &Path,
    queue: &mut VecDeque<PathBuf>,
) -> Option<()> {
    let table = value.as_table()?;
    for key in [
        "dependencies",
        "dev-dependencies",
        "dev_dependencies",
        "build-dependencies",
        "build_dependencies",
    ] {
        if let Some(dependencies) = table.get(key) {
            for dependency in dependencies.as_table()?.values() {
                match dependency {
                    toml::Value::String(_) => {}
                    toml::Value::Table(dependency) => {
                        if let Some(path) = dependency.get("path") {
                            queue
                                .push_back(join_relative(base, path.as_str()?)?.join("Cargo.toml"));
                        }
                    }
                    _ => return None,
                }
            }
        }
    }
    if let Some(workspace) = table.get("workspace") {
        collect_path_inputs(workspace, base, queue)?;
    }
    if let Some(targets) = table.get("target") {
        for target in targets.as_table()?.values() {
            collect_path_inputs(target, base, queue)?;
        }
    }
    for key in ["patch", "replace"] {
        if let Some(overrides) = table.get(key) {
            for entry in overrides.as_table()?.values() {
                let entries: Vec<_> = if key == "patch" {
                    entry.as_table()?.values().collect()
                } else {
                    vec![entry]
                };
                for dependency in entries {
                    if let Some(path) = dependency
                        .as_table()
                        .and_then(|dependency| dependency.get("path"))
                    {
                        queue.push_back(join_relative(base, path.as_str()?)?.join("Cargo.toml"));
                    }
                }
            }
        }
    }
    Some(())
}

fn is_link(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// テンプレートで任意トークン・item・属性を受け渡す宣言マクロは、配置変更を否定できない。
pub(super) fn dynamic_macro_template(root: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if node.kind() == "macro_definition" {
            let mut children = vec![node];
            while let Some(child) = children.pop() {
                if child.kind() == "fragment_specifier"
                    && child
                        .utf8_text(source)
                        .is_ok_and(|fragment| matches!(fragment, "tt" | "item" | "meta"))
                {
                    return true;
                }
                if child.kind() == "token_tree" {
                    let mut cursor = child.walk();
                    let tokens: Vec<_> = child
                        .children(&mut cursor)
                        .filter(|token| !matches!(token.kind(), "line_comment" | "block_comment"))
                        .collect();
                    for (index, token) in tokens.iter().enumerate() {
                        if token.kind() == "metavariable"
                            && (tokens.get(index + 1).is_some_and(|next| next.kind() == "!")
                                || tokens.first().is_some_and(|first| first.kind() == "[")
                                || tokens
                                    .get(index.wrapping_sub(1))
                                    .is_some_and(|previous| previous.kind() == "#"))
                        {
                            return true;
                        }
                    }
                }
                let mut cursor = child.walk();
                children.extend(child.named_children(&mut cursor));
            }
            continue;
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    false
}
