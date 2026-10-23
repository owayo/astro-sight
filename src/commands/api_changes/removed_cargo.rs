//! 削除した Rust の自由関数と同名の出現のうち、Cargo の依存で削除元の package を名前で指せない
//! ものを見分ける (Issue #111)。変更前のツリー (base の git tree) と変更後のツリー (作業ツリー。
//! staged は元の Cargo 設定を確かめられないため対象外) の両方で証明できた出現だけを削除判定から外し、証明できなければ従来
//! どおり参照に数える。

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;

use crate::commands::git_input::GitBlobBatch;
use crate::engine::impact::rust_cargo_config::CargoConfigProof;
use crate::engine::impact::rust_cargo_reachability::{
    CargoIndex, TreeEntry, WorkspaceFiles, WorkspaceTree, lexical_relative, same_package_name,
};
use crate::engine::parser;
use crate::git_support::validate_git_revision;
use crate::language::LangId;
use crate::models::impact::DiffFile;

/// git の tree の要素数・`ls-tree` の出力・1 つの blob・読む blob の合計の上限。合計は走査と、
/// crate 名を確かめる読み直しで別に数える。超えたら証明しない (要素数と blob は removed_ffi の
/// 旧ツリー走査と同じ)。
const MAX_TREE_ENTRIES: usize = 10_000;
const MAX_LISTING_BYTES: usize = 4 * 1024 * 1024;
const MAX_BLOB_BYTES: usize = 2 * 1024 * 1024;
const MAX_TREE_BYTES: usize = 64 * 1024 * 1024;

/// 削除候補の分類 1 回で共有する、変更前と変更後のツリーの Cargo の index。
pub(crate) struct RemovedRustPackages<'a> {
    dir: &'a str,
    base: &'a GitBlobBatch,
    config: CargoConfigProof,
    physical_root: Option<PathBuf>,
    /// diff の rename の新しいパスから古いパス。
    renamed_from: HashMap<&'a str, &'a str>,
    /// diff の rename の古いパスから新しいパス。
    renamed_to: HashMap<&'a str, &'a str>,
    /// diff に hunk が出た (追加を含む) ファイルの新しいパス。
    changed: HashSet<&'a str>,
    old_tree: OnceCell<Option<ScannedTree<'a>>>,
    new_tree: OnceCell<Option<ScannedTree<'a>>>,
    proofs: RefCell<HashMap<(String, String), Option<Proof>>>,
    mentions: RefCell<HashMap<(usize, Vec<String>), bool>>,
    /// 参照ファイルごとの、変更前後で中身が同じか (削除元が複数でも読み直さない)。
    same_contents: RefCell<HashMap<String, bool>>,
}

#[derive(Clone, Copy)]
struct Proof {
    /// 出現のファイルを、この diff で書いた (新しく作った・内容を変えた) か、同じ中身と
    /// 確かめられない。
    touched: bool,
}

impl<'a> RemovedRustPackages<'a> {
    /// `renames` は diff_files に現れない内容同一の rename。
    pub(super) fn new(
        dir: &'a str,
        base: &'a GitBlobBatch,
        diff_files: &'a [DiffFile],
        renames: &'a [DiffFile],
    ) -> Self {
        let renames = diff_files.iter().chain(renames).filter(|file| {
            file.old_path != file.new_path
                && file.old_path != "/dev/null"
                && file.new_path != "/dev/null"
        });
        Self {
            dir,
            base,
            config: CargoConfigProof::capture(Path::new(dir)),
            physical_root: std::fs::canonicalize(dir).ok(),
            renamed_from: renames
                .clone()
                .map(|file| (file.new_path.as_str(), file.old_path.as_str()))
                .collect(),
            renamed_to: renames
                .map(|file| (file.old_path.as_str(), file.new_path.as_str()))
                .collect(),
            changed: diff_files
                .iter()
                .filter(|file| file.new_path != "/dev/null")
                .map(|file| file.new_path.as_str())
                .collect(),
            old_tree: OnceCell::new(),
            new_tree: OnceCell::new(),
            proofs: RefCell::default(),
            mentions: RefCell::default(),
            same_contents: RefCell::default(),
        }
    }

    /// 変更後のツリーの定義 `definition` が、削除元 `removed` の package を名前で指し得ないか。
    pub(super) fn excludes_definition(&self, definition: &str, removed: &str) -> bool {
        self.prove(definition, removed).is_some()
    }

    /// 変更後のツリーの参照 `reference` が、削除元 `removed` の宣言を名前で指し得ないか。
    /// この diff で書いたファイルの参照は、消した関数の呼び出しを写したまま束縛先を失った
    /// ものかもしれないので、同じファイル直下の同名の関数に解決すると分かる場合
    /// (`binds_in_file`) だけ外す。
    pub(super) fn excludes_reference(
        &self,
        reference: &str,
        removed: &str,
        binds_in_file: bool,
    ) -> bool {
        self.prove(reference, removed)
            .is_some_and(|proof| binds_in_file || !proof.touched)
    }

    fn prove(&self, path: &str, removed: &str) -> Option<Proof> {
        let key = (path.to_owned(), removed.to_owned());
        if let Some(proof) = self.proofs.borrow().get(&key) {
            return *proof;
        }
        let proof = self.prove_uncached(path, removed);
        self.proofs.borrow_mut().insert(key, proof);
        proof
    }

    fn prove_uncached(&self, path: &str, removed: &str) -> Option<Proof> {
        // 祖先のリンクの前後と Cargo の環境から、依存にない extern が注入され得る。
        if !self.config.keeps_dependency_sources(Path::new(self.dir))
            || !self
                .config
                .keeps_dependency_sources(self.physical_root.as_deref()?)
        {
            return None;
        }
        let old = self.old_tree()?;
        let target = old.index.owner(removed)?;
        let old_path = self.renamed_from.get(path).copied().unwrap_or(path);
        let existed = old.paths.contains(old_path);
        match old.index.owner(old_path) {
            Some(owner) if owner == target || old.index.may_reach(owner, target) => return None,
            // 変更前のツリーにあったのに、どの package の src にも属さない。
            None if existed => return None,
            _ => {}
        }
        let new = self.new_tree()?;
        let owner = new.index.owner(path)?;
        let reachable = new.index.reachable(owner);
        let counterparts = self.counterparts(old, target, removed, new)?;
        if reachable
            .iter()
            .any(|package| counterparts.contains(package))
        {
            return None;
        }
        // 同名の外部依存 (registry や git) は、消した package を公開した版かもしれない。
        let name = old.index.name(target);
        if reachable.iter().any(|&package| {
            new.index
                .external_dependencies(package)
                .iter()
                .any(|dependency| same_package_name(dependency, name))
        }) {
            return None;
        }
        // 依存を書き忘れたまま残った古い呼び出し (`old_app::run()`) を外さない。
        let crate_names = old.index.crate_names(target);
        if reachable
            .iter()
            .any(|&package| self.mentions(new, package, &crate_names))
        {
            return None;
        }
        // `-diff` の属性などで hunk が出ないファイルもあるので、diff に無いファイルは変更前後の
        // 中身を比べる。
        let touched =
            !existed || self.changed.contains(path) || !self.same_content(old_path, path, new);
        Some(Proof { touched })
    }

    /// 変更前のツリーの `old_path` と変更後のツリーの `path` が、改行コードの違いを除いて同じ
    /// 中身か。読めなければ違うとみなす。
    fn same_content(&self, old_path: &str, path: &str, new: &ScannedTree<'_>) -> bool {
        if let Some(&same) = self.same_contents.borrow().get(path) {
            return same;
        }
        let same = match (
            self.base.read_limited(old_path, MAX_BLOB_BYTES),
            new.read(Path::new(path)),
        ) {
            (Some(old), Some(current)) => {
                normalize_line_endings(&old) == normalize_line_endings(&current)
            }
            _ => false,
        };
        self.same_contents
            .borrow_mut()
            .insert(path.to_owned(), same);
        same
    }

    /// 変更前の package `target` に当たる、変更後のツリーの package。rename 先が package に
    /// 属さず対応を決められなければ None。
    fn counterparts(
        &self,
        old: &ScannedTree<'_>,
        target: usize,
        removed: &str,
        new: &ScannedTree<'_>,
    ) -> Option<HashSet<usize>> {
        let directory = old.index.directory(target);
        let mut counterparts: HashSet<usize> =
            new.index.packages_named(old.index.name(target)).collect();
        counterparts.extend(new.index.package_at(directory));
        counterparts.extend(new.index.owner(removed));
        let manifest = slash_path(&directory.join("Cargo.toml"))?;
        if let Some(moved) = self.renamed_to.get(manifest.as_str()) {
            counterparts.insert(new.index.package_at(Path::new(moved).parent()?)?);
        }
        if let Some(moved) = self.renamed_to.get(removed) {
            counterparts.insert(new.index.owner(moved)?);
        }
        Some(counterparts)
    }

    /// 変更後のツリーで `package` の src の Rust ソースが `names` のどれかに識別子として触れるか。
    fn mentions(&self, tree: &ScannedTree<'_>, package: usize, names: &[String]) -> bool {
        let key = (package, names.to_vec());
        if let Some(&found) = self.mentions.borrow().get(&key) {
            return found;
        }
        let found = tree.index.rust_files().iter().any(|path| {
            path.to_str().and_then(|text| tree.index.owner(text)) == Some(package)
                && tree
                    .read(path)
                    .is_none_or(|source| mentions_identifier(&source, names))
        });
        self.mentions.borrow_mut().insert(key, found);
        found
    }

    fn old_tree(&self) -> Option<&ScannedTree<'a>> {
        self.old_tree
            .get_or_init(|| scan_git_tree(self.dir, Blobs::Base(self.base)))
            .as_ref()
    }

    /// staged の解析は index を展開した一時リポジトリで行う。展開は symlink を書き出さず、
    /// gitlink を空のディレクトリにするので、ファイルシステムではなく index の tree を読む。
    fn new_tree(&self) -> Option<&ScannedTree<'a>> {
        self.new_tree
            .get_or_init(|| match crate::git_support::index_snapshot_tree(self.dir) {
                Some(tree) => {
                    scan_git_tree(self.dir, Blobs::Index(GitBlobBatch::new(self.dir, &tree)))
                }
                None => Some(ScannedTree {
                    index: CargoIndex::scan_directory(Path::new(self.dir), 1)?,
                    source: TreeSource::Directory(Path::new(self.dir)),
                    paths: HashSet::new(),
                }),
            })
            .as_ref()
    }
}

struct ScannedTree<'a> {
    index: CargoIndex,
    source: TreeSource<'a>,
    /// git の tree で列挙した要素のパス (`/` 区切り)。作業ツリーでは空。
    paths: HashSet<String>,
}

impl ScannedTree<'_> {
    fn read(&self, path: &Path) -> Option<Vec<u8>> {
        match &self.source {
            TreeSource::Directory(root) => {
                crate::engine::bounded_read::read_input_file(&root.join(path))
            }
            TreeSource::Git(tree) => tree.read(path),
        }
    }
}

enum TreeSource<'a> {
    Directory(&'a Path),
    Git(GitTree<'a>),
}

enum Blobs<'a> {
    Base(&'a GitBlobBatch),
    Index(GitBlobBatch),
}

struct GitTree<'a> {
    blobs: Blobs<'a>,
    /// まだ読める blob の合計。
    budget: Cell<usize>,
}

impl GitTree<'_> {
    fn blobs(&self) -> &GitBlobBatch {
        match &self.blobs {
            Blobs::Base(blobs) => blobs,
            Blobs::Index(blobs) => blobs,
        }
    }
}

impl WorkspaceTree for GitTree<'_> {
    fn read(&self, path: &Path) -> Option<Vec<u8>> {
        // 残りの予算より大きい blob は中身を確保せずに読み捨てる。
        let remaining = self.budget.get();
        let content = self
            .blobs()
            .read_limited(&slash_path(path)?, MAX_BLOB_BYTES.min(remaining))?;
        self.budget.set(remaining - content.len());
        // LFS のポインタは実際の中身を表さない。
        (!super::removed_ffi::is_lfs_pointer(&content)).then_some(content)
    }

    fn resolve(&self, base: &Path, relative: &str) -> Option<PathBuf> {
        lexical_relative(base, relative)
    }
}

fn scan_git_tree<'a>(dir: &str, blobs: Blobs<'a>) -> Option<ScannedTree<'a>> {
    let tree = GitTree {
        blobs,
        budget: Cell::new(MAX_TREE_BYTES),
    };
    let mut paths = HashSet::new();
    let index = CargoIndex::scan(&tree, || list_git_tree(dir, &tree, &mut paths), 1)?;
    tree.budget.set(MAX_TREE_BYTES);
    Some(ScannedTree {
        index,
        source: TreeSource::Git(tree),
        paths,
    })
}

/// `git ls-tree -r -z` を dir で実行し、dir 配下の要素を dir 相対のパスで列挙する。
fn list_git_tree(
    dir: &str,
    tree: &GitTree<'_>,
    paths: &mut HashSet<String>,
) -> Option<WorkspaceFiles> {
    let rev = tree.blobs().rev();
    validate_git_revision(rev, "git revision").ok()?;
    let mut child = crate::git_support::git_command_for_dir(dir)
        .args(["ls-tree", "-r", "-z"])
        .arg(rev)
        .current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let output = crate::commands::common::read_bytes_limited_and_drain(
        child.stdout.take()?,
        MAX_LISTING_BYTES,
        "Cargo tree listing",
    );
    if !child.wait().ok()?.success() {
        return None;
    }
    let output = output.ok()?;
    let mut entries = Vec::new();
    for raw in output
        .split(|byte| *byte == 0)
        .filter(|raw| !raw.is_empty())
    {
        let (metadata, path) = std::str::from_utf8(raw).ok()?.split_once('\t')?;
        let mut fields = metadata.split(' ');
        let (Some(mode), Some(kind), Some(_), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return None;
        };
        // Windows では `\` も区切りになり、tree の要素と別のパスを指してしまう。改行を含む
        // パスは cat-file に要求できず、上限の無い読み出しに落ちる。
        if entries.len() == MAX_TREE_ENTRIES || path.contains(['\\', '\n', '\r']) {
            return None;
        }
        entries.push((mode, kind, path));
    }
    let regular: HashSet<&str> = entries
        .iter()
        .filter(|(mode, kind, _)| *kind == "blob" && matches!(*mode, "100644" | "100755"))
        .map(|&(_, _, path)| path)
        .collect();
    let mut files = WorkspaceFiles::default();
    for &(mode, kind, path) in &entries {
        paths.insert(path.to_owned());
        let relative: PathBuf = Path::new(path).components().collect();
        if !WorkspaceFiles::admits(&relative)? {
            continue;
        }
        match (mode, kind) {
            ("100644" | "100755", "blob") => files.add(&relative, TreeEntry::File)?,
            ("120000", "blob") => {
                // リンク先をツリー内の通常ファイルと確かめられる symlink だけを受け入れる。
                let target = tree.blobs().read_limited(path, 4096)?;
                let target =
                    lexical_relative(relative.parent()?, std::str::from_utf8(&target).ok()?)?;
                if !regular.contains(slash_path(&target)?.as_str()) {
                    return None;
                }
                files.add(
                    &relative,
                    TreeEntry::Symlink {
                        to_directory: false,
                    },
                )?;
            }
            // gitlink (submodule) の中身は、この tree からは読めない。
            _ => return None,
        }
    }
    Some(files)
}

/// root 相対のパスを `/` 区切りの文字列にする (diff・refs・git の tree のパスと同じ形)。
fn slash_path(path: &Path) -> Option<String> {
    let parts: Option<Vec<&str>> = path
        .components()
        .map(|component| match component {
            Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .collect();
    Some(parts?.join("/"))
}

fn normalize_line_endings(content: &[u8]) -> Vec<u8> {
    let mut normalized = Vec::with_capacity(content.len());
    for (ix, &byte) in content.iter().enumerate() {
        if byte != b'\r' || content.get(ix + 1) != Some(&b'\n') {
            normalized.push(byte);
        }
    }
    normalized
}

/// `names` のどれかを識別子 (raw identifier を含む) として含むか。解析できなければ含むとみなす。
fn mentions_identifier(source: &[u8], names: &[String]) -> bool {
    if !names
        .iter()
        .any(|name| memchr::memmem::find(source, name.as_bytes()).is_some())
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
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if node.kind() == "identifier"
            && node.utf8_text(source).is_ok_and(|text| {
                let text = text.strip_prefix("r#").unwrap_or(text);
                names.iter().any(|name| name == text)
            })
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
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::process::{Command, Stdio};

    use super::{Blobs, mentions_identifier, normalize_line_endings, scan_git_tree};
    use crate::commands::git_input::GitBlobBatch;

    fn git(repo: &Path, args: &[&str], input: &[u8]) -> String {
        let mut child = Command::new("git")
            .args(args)
            .current_dir(repo)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        std::io::Write::write_all(&mut child.stdin.take().unwrap(), input).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "git {args:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    /// `files` (パスと中身) の tree を、index を通さずに `git mktree` で組む。
    fn tree(repo: &Path, files: &[(&str, &str)]) -> String {
        let mut directories: BTreeMap<&str, Vec<(&str, &str)>> = BTreeMap::new();
        let mut listing = Vec::new();
        for &(path, content) in files {
            match path.split_once('/') {
                Some((directory, rest)) => {
                    directories
                        .entry(directory)
                        .or_default()
                        .push((rest, content));
                }
                None => {
                    let object = git(repo, &["hash-object", "-w", "--stdin"], content.as_bytes());
                    listing.extend_from_slice(format!("100644 blob {object}\t{path}\0").as_bytes());
                }
            }
        }
        for (directory, children) in directories {
            let object = tree(repo, &children);
            listing.extend_from_slice(format!("040000 tree {object}\t{directory}\0").as_bytes());
        }
        git(repo, &["mktree", "-z"], &listing)
    }

    fn scans(extra: &[(&str, &str)]) -> bool {
        let directory = tempfile::tempdir().unwrap();
        let repo = directory.path();
        git(repo, &["init", "-q"], b"");
        let mut files = vec![
            ("Cargo.toml", "[workspace]\nmembers = [\"app\"]\n"),
            (
                "app/Cargo.toml",
                "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
            ),
            ("app/src/lib.rs", "pub fn run() {}\n"),
        ];
        files.extend_from_slice(extra);
        let tree = tree(repo, &files);
        let dir = repo.to_str().unwrap();
        scan_git_tree(dir, Blobs::Index(GitBlobBatch::new(dir, &tree))).is_some()
    }

    #[test]
    fn git_tree_with_unrequestable_paths_is_not_scanned() {
        assert!(scans(&[]));
        assert!(scans(&[("notes/readme.txt", "note\n")]));
        // 改行を含むパスは cat-file に要求できず、上限の無い読み出しに落ちる。
        assert!(!scans(&[("notes/a\nb.txt", "note\n")]));
        assert!(!scans(&[("notes/a\rb.txt", "note\n")]));
    }

    #[test]
    fn line_endings_and_identifiers() {
        assert_eq!(normalize_line_endings(b"a\r\nb\rc\n"), b"a\nb\rc\n");
        let names = ["old_app".to_owned()];
        assert!(mentions_identifier(b"pub use old_app::*;\n", &names));
        assert!(mentions_identifier(
            b"fn f() { r#old_app::run() }\n",
            &names
        ));
        assert!(!mentions_identifier(
            b"// old_app\nfn f() { \"old_app\"; }\n",
            &names
        ));
        // 解析できないソースは触れているとみなす。
        assert!(mentions_identifier(b"fn f( { old_app\n", &names));
    }
}
