//! Rust の名前の参照について、Cargo の package の到達性を保守的に証明する。
//!
//! ある package のソースは、Cargo の依存で使えるようにしない限り、別の package の宣言を名前で
//! 参照できない。manifest を読めない、ソースの取り込み、独自のビルド構成などがあれば証明しない。
//! impact の候補 (作業ツリー) と、API 差分で削除した宣言の参照 (変更前と変更後のツリー、
//! `commands/api_changes/removed_cargo.rs`) にだけ使い、refs と dead-code には使わない。

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use tree_sitter::Node;

use crate::engine::bounded_read::read_input_file;
use crate::engine::parser;
use crate::language::LangId;

struct Package {
    directory: PathBuf,
    name: String,
    manifest: toml::Value,
    dependencies: Vec<usize>,
    /// ワークスペースのメンバーに解決しない依存 (registry や git) の package 名。
    external_dependencies: Vec<String>,
}

/// ワークスペースの package と、依存の種類を区別しない依存の辺。
pub(crate) struct CargoIndex {
    packages: Vec<Package>,
    rust_files: Vec<PathBuf>,
}

impl CargoIndex {
    /// 作業ツリーを走査する。package が `minimum_packages` 個に満たなければソースを読まずに諦める。
    pub(crate) fn scan_directory(root: &Path, minimum_packages: usize) -> Option<Self> {
        Self::scan(
            &DirectoryTree(root),
            || walk_directory(root),
            minimum_packages,
        )
    }

    /// root の manifest を確かめてから `list` でツリーを列挙し、index を作る。
    ///
    /// The subset accepted here is intentionally smaller than Cargo's manifest
    /// language. Every unhandled form makes the proof unavailable.
    pub(crate) fn scan(
        tree: &dyn WorkspaceTree,
        list: impl FnOnce() -> Option<WorkspaceFiles>,
        minimum_packages: usize,
    ) -> Option<Self> {
        let root_value = read_manifest(tree, Path::new("Cargo.toml"))?;
        let root_manifest = root_value.get("workspace")?.as_table()?.clone();
        if root_manifest.get("exclude").is_some() {
            return None;
        }
        let members = root_manifest.get("members")?.as_array()?;
        let mut member_directories = HashSet::new();
        for member in members {
            let member = member.as_str()?;
            if member.contains(['*', '?', '[', ']']) {
                return None;
            }
            member_directories.insert(tree.resolve(Path::new(""), member)?);
        }

        let files = list()?;
        let mut manifests = vec![(PathBuf::new(), root_value)];
        for path in &files.manifests {
            manifests.push((path.parent()?.to_owned(), read_manifest(tree, path)?));
        }
        let manifest_directories: HashSet<&Path> = manifests
            .iter()
            .map(|(directory, _)| directory.as_path())
            .collect();
        if member_directories
            .iter()
            .any(|directory| !manifest_directories.contains(directory.as_path()))
        {
            return None;
        }

        let mut packages = Vec::new();
        for (directory, manifest) in manifests {
            if manifest.get("patch").is_some() || manifest.get("replace").is_some() {
                return None;
            }
            if directory.as_os_str().is_empty() {
                if manifest.get("package").is_none() {
                    continue;
                }
            } else if !member_directories.contains(&directory)
                || manifest.get("workspace").is_some()
            {
                return None;
            }
            let package = manifest.get("package")?;
            if package.get("workspace").is_some() || custom_target_path(&manifest) {
                return None;
            }
            let name = package.get("name")?.as_str()?.to_owned();
            // Sysroot crates can be visible without an ordinary Cargo dependency.
            if matches!(name.as_str(), "std" | "core" | "alloc") {
                return None;
            }
            packages.push(Package {
                directory,
                name,
                manifest,
                dependencies: Vec::new(),
                external_dependencies: Vec::new(),
            });
        }
        if packages.len() < minimum_packages {
            return None;
        }
        let mut by_name = HashMap::new();
        let mut by_directory = HashMap::new();
        for (ix, package) in packages.iter().enumerate() {
            if by_name.insert(package.name.clone(), ix).is_some()
                || by_directory.insert(package.directory.clone(), ix).is_some()
            {
                return None;
            }
        }
        // A normal `mod` chain can compile another member's source when that
        // member lives inside this package's tree (including build.rs, tests, and
        // examples), without a macro or #[path].
        for (ix, outer) in packages.iter().enumerate() {
            if packages.iter().enumerate().any(|(other_ix, inner)| {
                other_ix != ix && inner.directory.starts_with(&outer.directory)
            }) {
                return None;
            }
        }

        let workspace_dependencies = root_manifest
            .get("dependencies")
            .and_then(toml::Value::as_table);
        for package in &mut packages {
            let dependencies = package_dependencies(
                tree,
                package,
                workspace_dependencies,
                &by_name,
                &by_directory,
            )?;
            let mut edges: Vec<usize> = dependencies.edges.into_iter().collect();
            edges.sort_unstable();
            let mut external: Vec<String> = dependencies.external.into_iter().collect();
            external.sort_unstable();
            package.dependencies = edges;
            package.external_dependencies = external;
        }
        // A source file may be compiled by a different package through #[path],
        // include!, or a macro. A path-based owner is safe only when no Rust source
        // in the workspace can introduce such a cross-package compilation context.
        for path in &files.rust_files {
            if source_has_expansion(&tree.read(path)?) {
                return None;
            }
        }
        Some(Self {
            packages,
            rust_files: files.rust_files,
        })
    }

    pub(crate) fn owner(&self, path: &str) -> Option<usize> {
        let path = Path::new(path);
        if !path.extension()?.to_str()?.eq_ignore_ascii_case("rs") {
            return None;
        }
        self.packages
            .iter()
            .enumerate()
            .filter(|(_, package)| path.starts_with(package.directory.join("src")))
            .max_by_key(|(_, package)| package.directory.components().count())
            .map(|(ix, _)| ix)
    }

    pub(crate) fn may_reach(&self, from: usize, target: usize) -> bool {
        let mut pending = vec![from];
        let mut seen = HashSet::new();
        while let Some(package) = pending.pop() {
            if package == target {
                return true;
            }
            if seen.insert(package) {
                pending.extend(self.packages[package].dependencies.iter().copied());
            }
        }
        false
    }

    /// `from` から依存でたどれる package (自身を含む、番号順)。
    pub(crate) fn reachable(&self, from: usize) -> Vec<usize> {
        let mut pending = vec![from];
        let mut seen = HashSet::new();
        while let Some(package) = pending.pop() {
            if seen.insert(package) {
                pending.extend(self.packages[package].dependencies.iter().copied());
            }
        }
        let mut reachable: Vec<usize> = seen.into_iter().collect();
        reachable.sort_unstable();
        reachable
    }

    pub(crate) fn name(&self, package: usize) -> &str {
        &self.packages[package].name
    }

    pub(crate) fn directory(&self, package: usize) -> &Path {
        &self.packages[package].directory
    }

    pub(crate) fn external_dependencies(&self, package: usize) -> &[String] {
        &self.packages[package].external_dependencies
    }

    /// ソースからこの package の library を指す名前 (`-` を `_` にした package 名と `[lib] name`)。
    pub(crate) fn crate_names(&self, package: usize) -> Vec<String> {
        let package = &self.packages[package];
        let mut names = vec![package.name.replace('-', "_")];
        if let Some(name) = package
            .manifest
            .get("lib")
            .and_then(|lib| lib.get("name"))
            .and_then(toml::Value::as_str)
        {
            names.push(name.replace('-', "_"));
        }
        names.sort_unstable();
        names.dedup();
        names
    }

    /// `directory` (root 相対) にある package。
    pub(crate) fn package_at(&self, directory: &Path) -> Option<usize> {
        self.packages
            .iter()
            .position(|package| package.directory == directory)
    }

    /// 名前が `name` と同じとみなせる package ([`same_package_name`])。
    pub(crate) fn packages_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = usize> + 'a {
        self.packages
            .iter()
            .enumerate()
            .filter(move |(_, package)| same_package_name(&package.name, name))
            .map(|(ix, _)| ix)
    }

    /// 走査した Rust ソース (root 相対)。
    pub(crate) fn rust_files(&self) -> &[PathBuf] {
        &self.rust_files
    }
}

/// crates.io と同じく、`-` と `_`・ASCII の大文字と小文字だけが違う package 名を同じとみなす。
pub(crate) fn same_package_name(left: &str, right: &str) -> bool {
    let fold = |byte: u8| {
        if byte == b'-' {
            b'_'
        } else {
            byte.to_ascii_lowercase()
        }
    };
    left.len() == right.len()
        && left
            .bytes()
            .zip(right.bytes())
            .all(|(left, right)| fold(left) == fold(right))
}

/// 証明に使うツリーの読み出し。作業ツリーと git の tree で同じ証明を使う。
pub(crate) trait WorkspaceTree {
    /// root 相対のファイルを読む。読めない・上限を超える・中身を確かめられないなら None。
    fn read(&self, path: &Path) -> Option<Vec<u8>>;
    /// `base` (root 相対のディレクトリ) から見た `relative` の root 相対のパス。root の外なら None。
    fn resolve(&self, base: &Path, relative: &str) -> Option<PathBuf>;
}

struct DirectoryTree<'a>(&'a Path);

impl WorkspaceTree for DirectoryTree<'_> {
    fn read(&self, path: &Path) -> Option<Vec<u8>> {
        read_input_file(&self.0.join(path))
    }

    fn resolve(&self, base: &Path, relative: &str) -> Option<PathBuf> {
        canonical_relative(self.0, base, relative)
    }
}

/// `base` (root 相対) から `relative` をたどった root 相対のパスを、ファイルシステムを見ずに
/// 求める。絶対パスと root より上へ出るパスは None。symlink を辿れないツリーで使う。
pub(crate) fn lexical_relative(base: &Path, relative: &str) -> Option<PathBuf> {
    let mut parts = Vec::new();
    for component in base.components().chain(Path::new(relative).components()) {
        match component {
            Component::Normal(name) => parts.push(name),
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(parts.into_iter().collect())
}

/// 列挙したディレクトリ以外の要素。ディレクトリは呼び出し側が辿る。
pub(crate) enum TreeEntry {
    File,
    Symlink { to_directory: bool },
}

/// 証明に使うファイル (root 相対)。root の Cargo.toml は列挙の前に読むので含めない。
#[derive(Default)]
pub(crate) struct WorkspaceFiles {
    manifests: Vec<PathBuf>,
    rust_files: Vec<PathBuf>,
}

impl WorkspaceFiles {
    /// `path` の各要素の名前で、辿る (true)・飛ばす (false)・証明しない (None) を決める。
    pub(crate) fn admits(path: &Path) -> Option<bool> {
        for (depth, component) in path.components().enumerate() {
            let Component::Normal(name) = component else {
                return None;
            };
            // Nested directories with these names can be ordinary Rust modules.
            // At the root, vendor and node_modules may contain unindexed crates
            // that compile workspace source, so their presence prevents proof.
            if depth == 0 {
                match name.to_str() {
                    Some(".git" | "target") => return Some(false),
                    Some("vendor" | "node_modules") => return None,
                    _ => {}
                }
            }
            if name
                .to_str()
                .is_some_and(|name| name.eq_ignore_ascii_case(".cargo"))
            {
                // rustflags can supply an extra --extern and break the manifest proof.
                return None;
            }
        }
        Some(true)
    }

    /// ディレクトリ以外の要素を 1 つ足す。証明を妨げる要素なら None。
    pub(crate) fn add(&mut self, path: &Path, entry: TreeEntry) -> Option<()> {
        if !Self::admits(path)? {
            return Some(());
        }
        match entry {
            TreeEntry::Symlink { to_directory } => {
                if to_directory
                    || path.extension().is_some_and(|ext| match ext.to_str() {
                        Some(ext) => {
                            !ext.is_ascii()
                                || ext.eq_ignore_ascii_case("rs")
                                || ext.eq_ignore_ascii_case("toml")
                        }
                        None => true,
                    })
                {
                    return None;
                }
            }
            TreeEntry::File => {
                if path
                    .file_name()
                    .and_then(OsStr::to_str)
                    .is_some_and(|name| name.eq_ignore_ascii_case("Cargo.toml"))
                {
                    if path
                        .parent()
                        .is_some_and(|parent| !parent.as_os_str().is_empty())
                    {
                        self.manifests.push(path.to_owned());
                    }
                } else if let Some(extension) = path.extension() {
                    let extension = extension.to_str()?;
                    // Case-insensitive filesystems can fold non-ASCII characters
                    // (for example long s) into `.rs` during module resolution.
                    if !extension.is_ascii() {
                        return None;
                    }
                    if extension.eq_ignore_ascii_case("rs") {
                        self.rust_files.push(path.to_owned());
                    }
                }
            }
        }
        Some(())
    }
}

fn walk_directory(root: &Path) -> Option<WorkspaceFiles> {
    let mut files = WorkspaceFiles::default();
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory).ok()? {
            let entry = entry.ok()?;
            let kind = entry.file_type().ok()?;
            let path = entry.path();
            let relative = path.strip_prefix(root).ok()?;
            if kind.is_symlink() {
                let to_directory = std::fs::metadata(&path).is_ok_and(|target| target.is_dir());
                files.add(relative, TreeEntry::Symlink { to_directory })?;
            } else if kind.is_dir() {
                if WorkspaceFiles::admits(relative)? {
                    pending.push(path);
                }
            } else if kind.is_file() {
                files.add(relative, TreeEntry::File)?;
            }
        }
    }
    Some(files)
}

fn read_manifest(tree: &dyn WorkspaceTree, path: &Path) -> Option<toml::Value> {
    toml::from_str(std::str::from_utf8(&tree.read(path)?).ok()?).ok()
}

/// One impact request owns one lazy index, shared by all reference workers.
pub(super) struct RustCargoReachability {
    root: PathBuf,
    enabled: bool,
    index: OnceLock<Option<CargoIndex>>,
}

impl RustCargoReachability {
    pub(super) fn new(root: &Path, enabled: bool) -> Self {
        Self {
            root: root.to_owned(),
            enabled,
            index: OnceLock::new(),
        }
    }

    pub(super) fn proves_unreachable(&self, reference: &str, changed: &str) -> bool {
        if !self.enabled {
            return false;
        }
        // package が 1 つなら別 package からの参照は無いので、ソースを読まずに諦める。
        let Some(index) = self
            .index
            .get_or_init(|| CargoIndex::scan_directory(&self.root, 2))
        else {
            return false;
        };
        let (Some(reference_owner), Some(changed_owner)) =
            (index.owner(reference), index.owner(changed))
        else {
            return false;
        };
        reference_owner != changed_owner && !index.may_reach(reference_owner, changed_owner)
    }
}

fn canonical_relative(root: &Path, base: &Path, relative: &str) -> Option<PathBuf> {
    let relative = Path::new(relative);
    if relative.is_absolute() {
        return None;
    }
    root.join(base)
        .join(relative)
        .canonicalize()
        .ok()?
        .strip_prefix(root.canonicalize().ok()?)
        .ok()
        .map(Path::to_owned)
}

fn custom_target_path(manifest: &toml::Value) -> bool {
    // A custom build script can point directly at a source file owned by a
    // different package, and build-dependencies can make its names available.
    if manifest
        .get("package")
        .and_then(|package| package.get("build"))
        .is_some_and(|build| !build.is_bool())
    {
        return true;
    }
    for key in ["lib", "bin", "example", "test", "bench"] {
        let Some(target) = manifest.get(key) else {
            continue;
        };
        let candidates: Vec<&toml::Value> = match target {
            toml::Value::Array(values) => values.iter().collect(),
            value => vec![value],
        };
        if candidates.iter().any(|target| target.get("path").is_some()) {
            return true;
        }
    }
    false
}

#[derive(Default)]
struct Dependencies {
    edges: HashSet<usize>,
    /// メンバーに解決しない依存の package 名。
    external: HashSet<String>,
}

fn package_dependencies(
    tree: &dyn WorkspaceTree,
    package: &Package,
    workspace_dependencies: Option<&toml::map::Map<String, toml::Value>>,
    by_name: &HashMap<String, usize>,
    by_directory: &HashMap<PathBuf, usize>,
) -> Option<Dependencies> {
    let mut dependencies = Dependencies::default();
    for key in [
        "dependencies",
        "dev-dependencies",
        "dev_dependencies",
        "build-dependencies",
        "build_dependencies",
    ] {
        if let Some(table) = package.manifest.get(key) {
            collect_dependencies(
                table,
                tree,
                &package.directory,
                workspace_dependencies,
                by_name,
                by_directory,
                &mut dependencies,
            )?;
        }
    }
    if let Some(targets) = package.manifest.get("target") {
        for target in targets.as_table()?.values() {
            for (key, table) in target.as_table()? {
                if !matches!(
                    key.as_str(),
                    "dependencies"
                        | "dev-dependencies"
                        | "dev_dependencies"
                        | "build-dependencies"
                        | "build_dependencies"
                ) {
                    return None;
                }
                collect_dependencies(
                    table,
                    tree,
                    &package.directory,
                    workspace_dependencies,
                    by_name,
                    by_directory,
                    &mut dependencies,
                )?;
            }
        }
    }
    Some(dependencies)
}

#[allow(clippy::too_many_arguments)]
fn collect_dependencies(
    table: &toml::Value,
    tree: &dyn WorkspaceTree,
    package_directory: &Path,
    workspace_dependencies: Option<&toml::map::Map<String, toml::Value>>,
    by_name: &HashMap<String, usize>,
    by_directory: &HashMap<PathBuf, usize>,
    dependencies: &mut Dependencies,
) -> Option<()> {
    for (key, value) in table.as_table()? {
        let (value, base) = match value.get("workspace") {
            Some(toml::Value::Boolean(true)) => {
                if value.get("package").is_some() || value.get("path").is_some() {
                    return None;
                }
                (workspace_dependencies?.get(key)?, Path::new(""))
            }
            Some(_) => return None,
            None => (value, package_directory),
        };
        let (name, path) = match value {
            toml::Value::String(_) => (key.as_str(), None),
            toml::Value::Table(_) => {
                let name = match value.get("package") {
                    Some(value) => value.as_str()?,
                    None => key,
                };
                let path = match value.get("path") {
                    Some(value) => Some(value.as_str()?),
                    None => None,
                };
                (name, path)
            }
            _ => return None,
        };
        if let Some(path) = path {
            let directory = tree.resolve(base, path)?;
            dependencies.edges.insert(*by_directory.get(&directory)?);
        }
        if let Some(&owner) = by_name.get(name) {
            dependencies.edges.insert(owner);
        } else if path.is_none() {
            dependencies.external.insert(name.to_owned());
        }
    }
    Some(())
}

fn source_has_expansion(source: &[u8]) -> bool {
    // LFS のポインタからは実際のソースの取り込みやマクロを確かめられない。
    if source.starts_with(b"version https://git-lfs.github.com/spec/v1") {
        return true;
    }
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
    has_expansion_node(root, source)
}

fn has_expansion_node(root: Node<'_>, source: &[u8]) -> bool {
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        let attribute = matches!(node.kind(), "attribute_item" | "inner_attribute_item");
        let derive = attribute
            && node
                .named_child(0)
                .and_then(|attribute| attribute.named_child(0))
                .and_then(|name| name.utf8_text(source).ok())
                == Some("derive");
        if node.kind() == "macro_invocation"
            || attribute
                && (derive || super::rust_std_origin::safe_attribute(node, source).is_none())
        {
            return true;
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    false
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    use super::{
        CargoIndex, RustCargoReachability, TreeEntry, WorkspaceFiles, WorkspaceTree,
        lexical_relative,
    };

    fn proves_unreachable(
        workspace_extra: &str,
        b_manifest_extra: &str,
        c_manifest_extra: &str,
        b_source: &str,
        enabled: bool,
    ) -> bool {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        for package in ["a", "b", "c"] {
            std::fs::create_dir_all(root.join(package).join("src")).unwrap();
        }
        std::fs::write(
            root.join("Cargo.toml"),
            format!("[workspace]\nmembers = [\"a\", \"b\", \"c\"]\n{workspace_extra}"),
        )
        .unwrap();
        for (package, extra) in [("a", ""), ("b", b_manifest_extra), ("c", c_manifest_extra)] {
            std::fs::write(
                root.join(package).join("Cargo.toml"),
                format!(
                    "[package]\nname = \"sample-{package}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{extra}"
                ),
            )
            .unwrap();
            std::fs::write(
                root.join(package).join("src/lib.rs"),
                if package == "b" {
                    b_source
                } else {
                    "pub struct Result;\n"
                },
            )
            .unwrap();
        }
        RustCargoReachability::new(root, enabled).proves_unreachable("b/src/lib.rs", "a/src/lib.rs")
    }

    #[test]
    fn unrelated_package_is_excluded_but_all_dependency_kinds_remain() {
        let ordinary = "pub fn unrelated() {}\n";
        assert!(proves_unreachable("", "", "", ordinary, true));
        for dependency in [
            "[dependencies]\nsample-a = { path = \"../a\" }\n",
            "[dependencies]\nrenamed = { package = \"sample-a\", path = \"../a\" }\n",
            "[dev-dependencies]\nsample-a = { path = \"../a\" }\n",
            "[dev_dependencies]\nsample-a = { path = \"../a\" }\n",
            "[build-dependencies]\nsample-a = { path = \"../a\" }\n",
            "[build_dependencies]\nsample-a = { path = \"../a\" }\n",
            "[target.'cfg(unix)'.dependencies]\nsample-a = { path = \"../a\" }\n",
            "[dependencies]\nsample-a = { path = \"../a\", optional = true }\n",
            "[dependencies]\nsample-a.workspace = true\n",
            "[dependencies]\nsample-c = { path = \"../c\" }\n",
        ] {
            let workspace = if dependency.contains("workspace = true") {
                "[workspace.dependencies]\nsample-a = { path = \"a\" }\n"
            } else {
                ""
            };
            let c = if dependency.contains("sample-c") {
                "[dependencies]\nsample-a = { path = \"../a\" }\n"
            } else {
                ""
            };
            assert!(
                !proves_unreachable(workspace, dependency, c, ordinary, true),
                "{dependency}"
            );
        }
    }

    #[test]
    fn uncertain_manifest_or_source_keeps_existing_impact() {
        let ordinary = "pub fn unrelated() {}\n";
        assert!(!proves_unreachable("", "", "", ordinary, false));
        assert!(!proves_unreachable(
            "[patch.crates-io]\nother = { path = \"c\" }\n",
            "",
            "",
            ordinary,
            true
        ));
        assert!(!proves_unreachable(
            "",
            "[dependencies]\nexternal = { path = \"../../outside\" }\n",
            "",
            ordinary,
            true
        ));
        assert!(!proves_unreachable(
            "",
            "",
            "",
            "include!(\"shared.rs\");\n",
            true
        ));
        assert!(!proves_unreachable(
            "",
            "",
            "",
            "#[path = \"shared.rs\"] mod shared;\n",
            true
        ));
        for source in [
            "version https://git-lfs.github.com/spec/v1\noid sha256:0\nsize 12\n",
            "version https://git-lfs.github.com/spec/v1\r\noid sha256:0\r\nsize 12\r\n",
        ] {
            assert!(!proves_unreachable("", "", "", source, true));
        }
        // An imported derive can shadow a built-in name such as Clone.
        assert!(!proves_unreachable(
            "",
            "[dependencies]\ncustom = \"1\"\n",
            "",
            "use custom::Clone;\n#[derive(Clone)] pub struct Value;\n",
            true
        ));
        assert!(!proves_unreachable(
            "",
            "[lib]\npath = \"src/lib.rs\"\n",
            "",
            ordinary,
            true
        ));
        assert!(!proves_unreachable(
            "",
            "build = \"../a/src/lib.rs\"\n",
            "",
            ordinary,
            true
        ));
    }

    #[test]
    fn source_inclusion_in_a_transitive_dependency_keeps_the_caller() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"a\", \"b\", \"c\"]\n",
        )
        .unwrap();
        for (directory, name, dependency, source) in [
            ("a", "sample-a", "", "pub struct Result;\n"),
            (
                "b",
                "sample-b",
                "[dependencies]\nsample-c = { path = \"../c\" }\n",
                "use sample_c::Result;\n",
            ),
            (
                "c",
                "sample-c",
                "",
                "#[path = \"../../a/src/lib.rs\"] mod shared; pub use shared::Result;\n",
            ),
        ] {
            std::fs::create_dir_all(root.join(directory).join("src")).unwrap();
            std::fs::write(
                root.join(directory).join("Cargo.toml"),
                format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n{dependency}"),
            )
            .unwrap();
            std::fs::write(root.join(directory).join("src/lib.rs"), source).unwrap();
        }
        assert!(
            !RustCargoReachability::new(root, true)
                .proves_unreachable("b/src/lib.rs", "a/src/lib.rs")
        );
    }

    #[test]
    fn skipped_root_directory_with_workspace_member_disables_proof() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"a\", \"b\", \"target/c\"]\n",
        )
        .unwrap();
        for (path, name) in [
            ("a", "sample-a"),
            ("b", "sample-b"),
            ("target/c", "sample-c"),
        ] {
            std::fs::create_dir_all(root.join(path).join("src")).unwrap();
            std::fs::write(
                root.join(path).join("Cargo.toml"),
                format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n"),
            )
            .unwrap();
            std::fs::write(root.join(path).join("src/lib.rs"), "pub struct Result;\n").unwrap();
        }
        assert!(
            !RustCargoReachability::new(root, true)
                .proves_unreachable("b/src/lib.rs", "a/src/lib.rs")
        );
    }

    #[test]
    fn package_nested_in_another_package_disables_proof() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"a\", \"d\", \"d/b\"]\n",
        )
        .unwrap();
        for (path, name) in [("a", "sample-a"), ("d", "sample-d"), ("d/b", "sample-b")] {
            std::fs::create_dir_all(root.join(path).join("src")).unwrap();
            std::fs::write(
                root.join(path).join("Cargo.toml"),
                format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n"),
            )
            .unwrap();
            std::fs::write(root.join(path).join("src/lib.rs"), "pub struct Result;\n").unwrap();
        }
        assert!(
            !RustCargoReachability::new(root, true)
                .proves_unreachable("d/b/src/lib.rs", "a/src/lib.rs")
        );
    }

    #[test]
    fn sysroot_named_package_never_proves_disconnection() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        for package in ["a", "b"] {
            std::fs::create_dir_all(root.join(package).join("src")).unwrap();
        }
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"a\", \"b\"]\n",
        )
        .unwrap();
        for (directory, name) in [("a", "core"), ("b", "sample-b")] {
            std::fs::write(
                root.join(directory).join("Cargo.toml"),
                format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n"),
            )
            .unwrap();
            std::fs::write(
                root.join(directory).join("src/lib.rs"),
                "pub struct Item;\n",
            )
            .unwrap();
        }
        assert!(
            !RustCargoReachability::new(root, true)
                .proves_unreachable("b/src/lib.rs", "a/src/lib.rs")
        );
    }

    /// 呼び出し側が列挙したツリー。git の tree と同じく symlink を辿らずに解決する。
    struct ListedTree(HashMap<PathBuf, &'static str>);

    impl WorkspaceTree for ListedTree {
        fn read(&self, path: &Path) -> Option<Vec<u8>> {
            self.0.get(path).map(|content| content.as_bytes().to_vec())
        }

        fn resolve(&self, base: &Path, relative: &str) -> Option<PathBuf> {
            lexical_relative(base, relative)
        }
    }

    fn scan_listed(files: &[(&str, &'static str)]) -> Option<CargoIndex> {
        let paths: Vec<PathBuf> = files
            .iter()
            .map(|(path, _)| Path::new(path).components().collect())
            .collect();
        let tree = ListedTree(
            paths
                .iter()
                .cloned()
                .zip(files.iter().map(|(_, content)| *content))
                .collect(),
        );
        CargoIndex::scan(
            &tree,
            || {
                let mut listed = WorkspaceFiles::default();
                for path in &paths {
                    listed.add(path, TreeEntry::File)?;
                }
                Some(listed)
            },
            1,
        )
    }

    #[test]
    fn listed_tree_keeps_package_identity_and_external_dependencies() {
        let index = scan_listed(&[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/core\", \"app\"]\n",
            ),
            (
                "crates/core/Cargo.toml",
                "[package]\nname = \"old-core\"\nversion = \"0.1.0\"\n[lib]\nname = \"legacy\"\n",
            ),
            ("crates/core/src/lib.rs", "pub fn run() {}\n"),
            (
                "app/Cargo.toml",
                "[package]\nname = \"app\"\nversion = \"0.1.0\"\n[dependencies]\nbase = { package = \"old-core\", path = \"../crates/core\" }\nserde = \"1\"\nlegacy = { package = \"Old_Core\", git = \"https://example.com/core\" }\n",
            ),
            ("app/src/main.rs", "fn main() {}\n"),
        ])
        .unwrap();
        let core = index.owner("crates/core/src/lib.rs").unwrap();
        let app = index.owner("app/src/main.rs").unwrap();
        assert!(index.may_reach(app, core) && !index.may_reach(core, app));
        let mut both = vec![app, core];
        both.sort_unstable();
        assert_eq!(index.reachable(app), both);
        assert_eq!(index.reachable(core), [core]);
        assert_eq!(index.name(core), "old-core");
        assert_eq!(index.directory(core), Path::new("crates/core"));
        assert_eq!(index.crate_names(core), ["legacy", "old_core"]);
        assert_eq!(index.package_at(Path::new("crates/core")), Some(core));
        assert_eq!(index.packages_named("OLD_core").collect::<Vec<_>>(), [core]);
        assert_eq!(index.external_dependencies(app), ["Old_Core", "serde"]);
        assert_eq!(index.rust_files().len(), 2);
    }

    #[test]
    fn single_package_and_missing_path_dependency() {
        let workspace = "[workspace]\nmembers = [\"app\"]\n";
        let manifest = "[package]\nname = \"app\"\nversion = \"0.1.0\"\n";
        assert!(
            scan_listed(&[
                ("Cargo.toml", workspace),
                ("app/Cargo.toml", manifest),
                ("app/src/lib.rs", ""),
            ])
            .is_some()
        );
        // 消したディレクトリを指し続ける path 依存は証明しない。
        assert!(
            scan_listed(&[
                ("Cargo.toml", workspace),
                (
                    "app/Cargo.toml",
                    "[package]\nname = \"app\"\nversion = \"0.1.0\"\n[dependencies]\nold = { path = \"../old\" }\n",
                ),
                ("app/src/lib.rs", ""),
            ])
            .is_none()
        );
        // impact は package が 1 つならソースを読まずに諦める。
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::create_dir_all(root.join("app/src")).unwrap();
        std::fs::write(root.join("Cargo.toml"), workspace).unwrap();
        std::fs::write(root.join("app/Cargo.toml"), manifest).unwrap();
        std::fs::write(root.join("app/src/lib.rs"), "").unwrap();
        assert!(CargoIndex::scan_directory(root, 2).is_none());
        assert!(CargoIndex::scan_directory(root, 1).is_some());
    }

    #[test]
    fn workspace_files_follow_the_directory_walk_rules() {
        assert_eq!(
            WorkspaceFiles::admits(Path::new("target/debug/build.rs")),
            Some(false)
        );
        assert_eq!(
            WorkspaceFiles::admits(Path::new("src/target/mod.rs")),
            Some(true)
        );
        assert_eq!(
            WorkspaceFiles::admits(Path::new("vendor/crate/src/lib.rs")),
            None
        );
        assert_eq!(
            WorkspaceFiles::admits(Path::new("src/vendor/mod.rs")),
            Some(true)
        );
        assert_eq!(
            WorkspaceFiles::admits(Path::new("a/.Cargo/config.toml")),
            None
        );
        let mut files = WorkspaceFiles::default();
        for path in ["Cargo.toml", "a/Cargo.toml", "a/src/lib.RS", "a/README"] {
            assert!(
                files.add(Path::new(path), TreeEntry::File).is_some(),
                "{path}"
            );
        }
        let link = |to_directory| TreeEntry::Symlink { to_directory };
        assert!(files.add(Path::new("AGENTS.md"), link(false)).is_some());
        assert!(files.add(Path::new("docs"), link(true)).is_none());
        assert!(files.add(Path::new("a/src/link.rs"), link(false)).is_none());
        assert!(
            files
                .add(Path::new("a/src/inc.rſ"), TreeEntry::File)
                .is_none()
        );
        assert_eq!(files.manifests, [PathBuf::from("a/Cargo.toml")]);
        assert_eq!(files.rust_files, [PathBuf::from("a/src/lib.RS")]);
    }

    #[test]
    fn lexical_relative_stays_inside_the_root() {
        assert_eq!(
            lexical_relative(Path::new("app"), "../crates/./core"),
            Some(Path::new("crates").join("core"))
        );
        assert_eq!(
            lexical_relative(Path::new(""), "app"),
            Some(PathBuf::from("app"))
        );
        assert_eq!(lexical_relative(Path::new("app"), "../../outside"), None);
        assert_eq!(lexical_relative(Path::new("app"), "/absolute"), None);
    }
}
