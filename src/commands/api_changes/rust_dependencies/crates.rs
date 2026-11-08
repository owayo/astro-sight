//! 差分の中の package の lib crate を、版 (旧 = base、新 = 作業ツリー) ごとに開く (Issue #300)。
//!
//! module のファイルは、crate root (`src/lib.rs`) から `mod` 宣言をたどって引く: inline の module、
//! `name.rs` と `name/mod.rs` (両方あるか、どちらも無ければ決められない)、静的な `#[path = "…"]`
//! (Rust reference の規則。`#[path]` で開いたファイルは `mod.rs` と同じ扱い)。`cfg_attr` の中の
//! `path` や、`#[path]` の付いた inline module の中の `mod x;` は決められない。差分に無いパスは旧新で
//! 同じ中身なので、索引を旧新で共有する。

use super::*;
use std::cell::{OnceCell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// 開いたファイルの鍵 (パス・module の経路・`mod.rs` と同じ扱いか)。
type FileKey = (String, Vec<String>, bool);

/// ファイルを開いた結果 (共有する)。
#[derive(Clone)]
enum Cached {
    File(File),
    /// 構文エラー。
    Unparsed,
    /// 無い・読めない。
    Missing,
}

/// crate root から `mod` 宣言をたどってファイルを開く上限 (ファイル → module の逆引きの木)。
const MAX_TREE_FILES: usize = 10_000;

/// ファイル → (module の経路, `mod.rs` と同じ扱いか) の木。
type ModuleTree = HashMap<String, Vec<(Vec<String>, bool)>>;

/// module の経路を crate root から `mod` 宣言でたどった結果。
pub(crate) enum Located {
    /// 中身のファイルと、ファイルの中の inline module の経路。
    Found(File, Vec<String>),
    /// その経路の module が無い。
    Absent,
    /// たどれない (場所を決められない・読めない・構文エラー・条件で分けた別々の中身)。
    Unknown,
}

/// 版の、package の lib crate。
pub(crate) struct CrateVersion<'a> {
    /// lib root のパス (`dir` 相対)。
    root: String,
    /// この版のパスの中身 (無ければ None)。
    read: SourceReader<'a>,
    /// 差分の中のパス (旧新とも)。ほかのパスは旧新で同じ中身なので、索引を旧新で共有する。
    changed: Rc<HashSet<String>>,
    /// 差分に無いファイルの索引 (旧新で共有)。
    shared: Rc<RefCell<HashMap<FileKey, Cached>>>,
    /// この版の差分の中のファイルの索引。
    own: RefCell<HashMap<FileKey, Cached>>,
    /// パスの有無。
    exists: RefCell<HashMap<String, bool>>,
    /// crate root から全ての `mod` 宣言をたどった木。必要になったときに作る。たどれなければ None。
    tree: OnceCell<Option<ModuleTree>>,
    /// ファイルの module の置き場と条件 (Issue #305)。必要になったときに作る。
    placements: OnceCell<super::placements::Placements>,
    /// 開いたファイルの読んだ量 (索引を作ったときだけ数える)。
    bytes: std::cell::Cell<usize>,
    /// 新しく開くファイルの大きさの上限 (1 つ, 合計)。超えるファイルは解析せず、開けないことにする
    /// (覚えない)。
    limit: std::cell::Cell<Option<(usize, usize)>>,
    /// 上限で開かなかったファイルがある。
    limited: std::cell::Cell<bool>,
}

impl<'a> CrateVersion<'a> {
    fn new(
        root: String,
        read: SourceReader<'a>,
        changed: Rc<HashSet<String>>,
        shared: Rc<RefCell<HashMap<FileKey, Cached>>>,
    ) -> Self {
        Self {
            root,
            read,
            changed,
            shared,
            own: RefCell::new(HashMap::new()),
            exists: RefCell::new(HashMap::new()),
            tree: OnceCell::new(),
            placements: OnceCell::new(),
            bytes: std::cell::Cell::new(0),
            limit: std::cell::Cell::new(None),
            limited: std::cell::Cell::new(false),
        }
    }

    /// crate root のパス (`dir` 相対)。
    pub(crate) fn root_path(&self) -> &str {
        &self.root
    }

    /// 新しく開くファイルの大きさの上限 (1 つ, 合計) を置く・外す。
    pub(crate) fn set_limit(&self, limit: Option<(usize, usize)>) {
        self.limit.set(limit);
    }

    /// 上限で開かなかったファイルがあるか。
    pub(crate) fn limited(&self) -> bool {
        self.limited.get()
    }

    /// ファイルの module の置き場と条件 (Issue #305)。
    pub(crate) fn placements(&self) -> &super::placements::Placements {
        self.placements
            .get_or_init(|| super::placements::walk(self))
    }

    /// パス `path` の中身 (この版)。
    pub(crate) fn read(&self, path: &str) -> Option<Vec<u8>> {
        (self.read)(path)
    }

    /// パス `path` がこの版にあるか。
    fn exists(&self, path: &str) -> bool {
        if let Some(&known) = self.exists.borrow().get(path) {
            return known;
        }
        let found = (self.read)(path).is_some();
        self.exists.borrow_mut().insert(path.to_owned(), found);
        found
    }

    /// パス `path` のファイルを、module の経路 `module` のファイルとして開く。
    fn open(&self, path: &str, module: Vec<String>, mod_rs: bool) -> Opened {
        let shared = !self.changed.contains(path);
        let key = (path.to_owned(), module, mod_rs);
        let cached = if shared {
            self.shared.borrow().get(&key).cloned()
        } else {
            self.own.borrow().get(&key).cloned()
        };
        let cached = if let Some(cached) = cached {
            cached
        } else {
            let source = (self.read)(path);
            // 上限を超えるファイルは解析せず、覚えない (上限を外した後の比較で開けるように)。
            if let (Some(source), Some((per_file, total))) = (&source, self.limit.get())
                && (source.len() > per_file || self.bytes.get() + source.len() > total)
            {
                self.limited.set(true);
                return Opened::Unknown;
            }
            let opened = match source {
                None => Cached::Missing,
                Some(source) => {
                    self.bytes.set(self.bytes.get() + source.len());
                    match DependencyIndex::new_in_module(&source, path, Some(key.1.clone()), mod_rs)
                    {
                        Some(index) => Cached::File(Rc::new(index)),
                        None => Cached::Unparsed,
                    }
                }
            };
            if shared {
                self.shared.borrow_mut().insert(key, opened.clone());
            } else {
                self.own.borrow_mut().insert(key, opened.clone());
            }
            opened
        };
        match cached {
            Cached::File(file) => Opened::File(file),
            // 差分に無いファイルの構文エラーは旧新で同じ。
            Cached::Unparsed if shared => Opened::Unparsed(path.to_owned()),
            _ => Opened::Unknown,
        }
    }

    /// module の経路 `module` を crate root から `mod` 宣言でたどる。
    pub(crate) fn locate_scope(&self, module: &[String]) -> Located {
        let Opened::File(mut file) = self.root() else {
            return Located::Unknown;
        };
        let mut scope: Vec<String> = Vec::new();
        for segment in module {
            let declarations = file.module_declarations(&scope, segment);
            if declarations.is_empty() {
                return Located::Absent;
            }
            if file.has_inline_module(&scope, segment) {
                // 条件で分けた別のファイルの宣言もあれば、どちらの中身か決められない。
                if declarations.iter().any(|declaration| !declaration.inline) {
                    return Located::Unknown;
                }
                scope.push(segment.clone());
                continue;
            }
            let mut found: Option<File> = None;
            for declaration in declarations {
                if declaration.inline {
                    return Located::Unknown;
                }
                match self.child(&file, &scope, segment, declaration) {
                    Opened::File(child) => {
                        if found
                            .as_ref()
                            .is_some_and(|known| known.path() != child.path())
                        {
                            return Located::Unknown;
                        }
                        found = Some(child);
                    }
                    _ => return Located::Unknown,
                }
            }
            let Some(child) = found else {
                return Located::Unknown;
            };
            file = child;
            scope.clear();
        }
        Located::Found(file, scope)
    }

    /// ファイル `path` を、この版の module のファイルとして開く。パスの規約の経路を crate root から
    /// たどってそのファイルに着けば、たどって開いた索引 (`#[path]` で開いたファイルの、中の `mod` の
    /// 探し方を保つ)。着かなければ crate root から全ての `mod` 宣言をたどった木で引く (`#[path]`)。1 つの
    /// module に決まらなければ Unknown。
    pub(crate) fn open_file(&self, path: &str) -> Opened {
        if let Some((_, module)) = source_module_path(path)
            && let Located::Found(file, scope) = self.locate_scope(&module)
            && scope.is_empty()
            && file.path() == path
        {
            return Opened::File(file);
        }
        match self.tree_entries(path).as_deref() {
            Some([(module, mod_rs)]) => self.open(path, module.clone(), *mod_rs),
            _ => Opened::Unknown,
        }
    }

    /// 木で引いた、ファイル `path` の module の経路と `mod.rs` と同じ扱いか (どの module にも使われて
    /// いなければ空)。木をたどれなければ None。
    fn tree_entries(&self, path: &str) -> Option<Vec<(Vec<String>, bool)>> {
        let tree = self.tree.get_or_init(|| self.walk_tree());
        Some(tree.as_ref()?.get(path).cloned().unwrap_or_default())
    }

    /// 木で引いた、ファイル `path` の module の経路 (どの module にも使われていなければ空)。木を
    /// たどれなければ None。
    pub(crate) fn tree_modules(&self, path: &str) -> Option<Vec<Vec<String>>> {
        Some(
            self.tree_entries(path)?
                .into_iter()
                .map(|(module, _)| module)
                .collect(),
        )
    }

    /// crate root から全ての `mod` 宣言をたどった、ファイル → module の経路。構文エラーのファイルも
    /// 入れる (中の宣言はたどれない)。場所を決められない宣言があるか、上限を超えれば None。
    fn walk_tree(&self) -> Option<ModuleTree> {
        let mut tree: ModuleTree = HashMap::new();
        let Opened::File(root) = self.root() else {
            return None;
        };
        let mut pending = vec![root];
        let mut seen: HashSet<(String, Vec<String>)> = HashSet::new();
        while let Some(file) = pending.pop() {
            let module = file.file_module()?.to_vec();
            if !seen.insert((file.path().to_owned(), module.clone())) {
                continue;
            }
            if seen.len() > MAX_TREE_FILES {
                return None;
            }
            tree.entry(file.path().to_owned())
                .or_default()
                .push((module.clone(), file.mod_rs));
            let scopes: Vec<Vec<String>> = file.scope_paths().cloned().collect();
            for scope in scopes {
                for name in file.scope_names(&scope) {
                    for declaration in file.module_declarations(&scope, &name) {
                        if declaration.inline {
                            continue;
                        }
                        let mut child_module = module.clone();
                        child_module.extend(scope.iter().cloned());
                        child_module.push(name.clone());
                        match self.child(&file, &scope, &name, declaration) {
                            Opened::File(child) => pending.push(child),
                            Opened::Unparsed(path) => {
                                let mod_rs = matches!(declaration.path, ModuleSource::Path(_))
                                    || path.ends_with("/mod.rs");
                                tree.entry(path).or_default().push((child_module, mod_rs));
                            }
                            Opened::Unknown => return None,
                        }
                    }
                }
            }
        }
        Some(tree)
    }
}

/// `dir` 相対のパス `path` の親のディレクトリ (無ければ空)。
fn parent_dir(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// ディレクトリ `dir` と相対のパス `relative` をつなぎ、`.`・`..` を畳む。`dir` の外 (リポジトリの外)
/// へ出るか、絶対パス・`\` を含むパスなら None。
fn join_path(dir: &str, relative: &str) -> Option<String> {
    if relative.is_empty()
        || relative.starts_with('/')
        || relative.contains('\\')
        || relative.contains(':')
    {
        return None;
    }
    let mut parts: Vec<&str> = dir.split('/').filter(|part| !part.is_empty()).collect();
    for part in relative.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    let path = parts.join("/");
    crate::engine::impact::is_safe_diff_path(&path).then_some(path)
}

impl CrateFiles for CrateVersion<'_> {
    fn root(&self) -> Opened {
        self.open(&self.root.clone(), Vec::new(), true)
    }

    fn child(
        &self,
        parent: &DependencyIndex,
        scope: &[String],
        name: &str,
        declaration: &ModuleDeclaration,
    ) -> Opened {
        let Some(parent_module) = parent.file_module() else {
            return Opened::Unknown;
        };
        // `#[path]` の付いた inline module の中の `mod x;` は、場所の規則を確かめていない。
        if parent.scope_has_path_attribute(scope) {
            return Opened::Unknown;
        }
        let mut module = parent_module.to_vec();
        module.extend(scope.iter().cloned());
        module.push(name.to_owned());
        let dir = parent_dir(parent.path());
        // 中の `mod x;` を探すディレクトリ: crate root・`mod.rs`・`#[path]` で開いたファイルはその
        // ディレクトリ、ほかはファイルの名前のディレクトリ。inline module の名前を続ける。
        let mut base = if parent.mod_rs {
            dir.to_owned()
        } else {
            let stem = parent
                .path()
                .rsplit('/')
                .next()
                .and_then(|file| file.strip_suffix(".rs"))
                .unwrap_or_default();
            if dir.is_empty() {
                stem.to_owned()
            } else {
                format!("{dir}/{stem}")
            }
        };
        for segment in scope {
            if !base.is_empty() {
                base.push('/');
            }
            base.push_str(segment);
        }
        match &declaration.path {
            ModuleSource::Unknown => Opened::Unknown,
            ModuleSource::Path(value) => {
                // ファイル直下の `#[path]` はファイルのディレクトリから、inline module の中は上の
                // ディレクトリから。
                let from = if scope.is_empty() { dir } else { base.as_str() };
                match join_path(from, value) {
                    Some(path) => self.open(&path, module, true),
                    None => Opened::Unknown,
                }
            }
            ModuleSource::Default => {
                let (Some(as_file), Some(as_mod)) = (
                    join_path(&base, &format!("{name}.rs")),
                    join_path(&base, &format!("{name}/mod.rs")),
                ) else {
                    return Opened::Unknown;
                };
                match (self.exists(&as_file), self.exists(&as_mod)) {
                    (true, false) => self.open(&as_file, module, false),
                    (false, true) => self.open(&as_mod, module, true),
                    _ => Opened::Unknown,
                }
            }
        }
    }
}

/// 旧新の lib crate と、束縛の変化 (Issue #300)。
pub(crate) struct CratePair<'a> {
    pub(crate) old: CrateVersion<'a>,
    pub(crate) new: CrateVersion<'a>,
    /// 束縛の変わり得る名前と module。
    pub(crate) changes: ChangeSet,
    /// 移した定義。
    pub(crate) moves: MoveMap,
    /// 変わった束縛を名指し得る、比べ直すファイル (旧いパス, 新しいパス)。差分に無いファイルと、
    /// 内容が同じ rename。
    pub(crate) dependents: Vec<(String, String)>,
    /// 比べ直すファイルを探し切れなかった理由。
    pub(crate) incomplete: Option<String>,
    /// 2 つ以上の module に使うファイル (どの module として比べるか決められない)。
    multi_placed: HashSet<String>,
    /// 束縛が旧新で変わり、上の束縛の変化と比べ直すファイルを求めた (Issue #300)。
    dependencies: bool,
    /// module の置き場の条件に効く入力 ([`DependencyIndex::module_condition_fingerprint`]) が旧新で
    /// 変わった (Issue #305)。
    pub(crate) module_conditions: bool,
    /// package (いちばん近い `Cargo.toml` のあるディレクトリ)。
    pub(crate) package: String,
}

impl CratePair<'_> {
    /// 比べるファイル (旧いパス `old_path`、新しいパス `new_path`) の旧新の索引。どちらかの版で module を
    /// 決められなければ None。
    pub(crate) fn current_files(&self, old_path: &str, new_path: &str) -> Option<(File, File)> {
        if self.multi_placed.contains(old_path) || self.multi_placed.contains(new_path) {
            return None;
        }
        let Opened::File(old) = self.old.open_file(old_path) else {
            return None;
        };
        let Opened::File(new) = self.new.open_file(new_path) else {
            return None;
        };
        Some((old, new))
    }

    /// 比べるファイルが、旧新のどちらかの版で lib crate の module になり得るか (木で引けて、どの module
    /// にも使われていなければなり得ない)。
    pub(crate) fn may_contain(&self, old_path: &str, new_path: &str) -> bool {
        self.multi_placed.contains(old_path)
            || self.multi_placed.contains(new_path)
            || self
                .old
                .tree_modules(old_path)
                .is_none_or(|modules| !modules.is_empty())
            || self
                .new
                .tree_modules(new_path)
                .is_none_or(|modules| !modules.is_empty())
    }
}

/// 差分の中の package の lib crate のうち、束縛か module の置き場の条件に効く入力が旧新で変わったもの
/// (検出 1 回の間に作る)。
pub(crate) struct RustCrates<'a> {
    pairs: HashMap<String, CratePair<'a>>,
    /// module の置き場の条件に効く入力が変わったが、lib crate を開けなかった package の crate root と
    /// 理由 (Issue #305)。
    module_gaps: Vec<(String, String)>,
}

/// パスが package の lib crate のソースになり得るなら、その package (いちばん近い `Cargo.toml` の
/// あるディレクトリ。途中の `src` という名前のディレクトリでは決めない)。別の target (`src/main.rs`・
/// `src/bin/`・`tests/`・`examples/`・`benches/`・`build.rs`) のソースでなければ、`src/` の外のファイル
/// (`#[path]` で読む) も含める。
pub(crate) fn crate_package(dir: &str, path: &str) -> Option<String> {
    if path == "/dev/null" || !path.ends_with(".rs") {
        return None;
    }
    let mut ancestor = parent_dir(path);
    let package = loop {
        let manifest = if ancestor.is_empty() {
            "Cargo.toml".to_owned()
        } else {
            format!("{ancestor}/Cargo.toml")
        };
        if std::path::Path::new(dir).join(&manifest).is_file() {
            break ancestor;
        }
        if ancestor.is_empty() {
            return None;
        }
        ancestor = parent_dir(ancestor);
    };
    let relative = if package.is_empty() {
        path
    } else {
        path.strip_prefix(&format!("{package}/"))?
    };
    let other_target = relative == "build.rs"
        || relative == "src/main.rs"
        || ["src/bin/", "tests/", "examples/", "benches/"]
            .iter()
            .any(|prefix| relative.starts_with(prefix));
    (!other_target).then(|| package.to_owned())
}

/// パスの package (いちばん近い `Cargo.toml` のあるディレクトリ)。別の target のソースも含める
/// (`#[path]` で lib が読み得る。lib の module かは置き場の索引で決める。Issue #305)。
pub(crate) fn package_of(dir: &str, path: &str) -> Option<String> {
    if path == "/dev/null" || !path.ends_with(".rs") {
        return None;
    }
    let mut ancestor = parent_dir(path);
    loop {
        let manifest = if ancestor.is_empty() {
            "Cargo.toml".to_owned()
        } else {
            format!("{ancestor}/Cargo.toml")
        };
        if std::path::Path::new(dir).join(&manifest).is_file() {
            return Some(ancestor.to_owned());
        }
        if ancestor.is_empty() {
            return None;
        }
        ancestor = parent_dir(ancestor);
    }
}

/// package の crate root のパス (`dir` 相対)。
fn library_root(package: &str) -> String {
    if package.is_empty() {
        "src/lib.rs".to_owned()
    } else {
        format!("{package}/src/lib.rs")
    }
}

impl<'a> RustCrates<'a> {
    /// 差分 `diff_files` の Rust のソースのある package ごとに、resolver が読む束縛が旧新で変わって
    /// いれば、旧新の lib crate を作り、束縛の変化と比べ直すファイルを求める。`renames` は hunk の無い
    /// 変更 (内容が同じ rename など)。旧新の版のファイルの有無と module の構成の変化に入れ、契約が
    /// 変わり得れば比べ直す。
    pub(crate) fn new(
        dir: &'a str,
        diff_files: &[crate::models::impact::DiffFile],
        renames: &[crate::models::impact::DiffFile],
        base_blobs: &'a crate::commands::git_input::GitBlobBatch,
    ) -> Self {
        let all: Vec<crate::models::impact::DiffFile> =
            diff_files.iter().chain(renames).cloned().collect();
        let rename_pairs: Vec<(String, String)> = renames
            .iter()
            .filter(|df| {
                df.old_path != df.new_path
                    && df.old_path != "/dev/null"
                    && df.new_path != "/dev/null"
            })
            .map(|df| (df.old_path.clone(), df.new_path.clone()))
            .collect();
        let mut packages: Vec<String> = all
            .iter()
            .flat_map(|df| [df.old_path.as_str(), df.new_path.as_str()])
            .filter_map(|path| package_of(dir, path))
            .collect();
        packages.sort();
        packages.dedup();
        let mut pairs = HashMap::new();
        let mut module_gaps = Vec::new();
        for package in packages {
            match build_pair(dir, &all, &rename_pairs, base_blobs, &package) {
                Built::Pair(pair) => {
                    pairs.insert(package, *pair);
                }
                Built::ModuleGap(reason) => module_gaps.push((library_root(&package), reason)),
                Built::Nothing => {}
            }
        }
        Self { pairs, module_gaps }
    }

    /// パス `path` の package の旧新の lib crate。束縛の入力が旧新で変わらない (ファイルの外へ出る
    /// パスの束縛は旧新で同じ)・lib が無いなら None。
    pub(crate) fn pair(&self, dir: &str, path: &str) -> Option<&CratePair<'a>> {
        self.pairs
            .get(&crate_package(dir, path)?)
            .filter(|pair| pair.dependencies)
    }

    /// 束縛が変わった package の、crate root のパスと旧新の lib crate。
    pub(crate) fn pairs(&self) -> impl Iterator<Item = (String, &CratePair<'a>)> {
        self.pairs
            .iter()
            .filter(|(_, pair)| pair.dependencies)
            .map(|(package, pair)| (library_root(package), pair))
    }

    /// module の置き場の条件に効く入力が変わった package の、crate root のパスと旧新の lib crate
    /// (Issue #305)。
    pub(crate) fn module_pairs(&self) -> impl Iterator<Item = (String, &CratePair<'a>)> {
        let mut pairs: Vec<(String, &CratePair<'a>)> = self
            .pairs
            .values()
            .filter(|pair| pair.module_conditions)
            .map(|pair| (pair.old.root_path().to_owned(), pair))
            .collect();
        pairs.sort_by(|left, right| left.0.cmp(&right.0));
        pairs.into_iter()
    }

    /// module の置き場の条件に効く入力が変わったが、lib crate を開けなかった package の crate root と
    /// 理由 (Issue #305)。
    pub(crate) fn module_gaps(&self) -> &[(String, String)] {
        &self.module_gaps
    }
}

/// package の旧新の lib crate を作った結果。
enum Built<'a> {
    Pair(Box<CratePair<'a>>),
    /// module の置き場の条件に効く入力が変わったが、lib crate を開けない (理由)。
    ModuleGap(String),
    Nothing,
}

/// package `package` の旧新の lib crate を作る。束縛が変われば束縛の変化と比べ直すファイルを求め
/// (Issue #300)、module の置き場の条件に効く入力が変われば印を付ける (Issue #305)。どちらも変わらない・
/// lib が旧新のどちらかに無い package は作らない。`[lib] path` で crate root を変えた package は扱わず、
/// module の置き場の条件に効く入力が変わっていれば開けなかったことを返す。
fn build_pair<'a>(
    dir: &'a str,
    diff_files: &[crate::models::impact::DiffFile],
    rename_pairs: &[(String, String)],
    base_blobs: &'a crate::commands::git_input::GitBlobBatch,
    package: &str,
) -> Built<'a> {
    let in_package = |path: &str| crate_package(dir, path).as_deref() == Some(package);
    let files: Vec<&crate::models::impact::DiffFile> = diff_files
        .iter()
        .filter(|df| in_package(&df.old_path) || in_package(&df.new_path))
        .collect();
    let module_files: Vec<&crate::models::impact::DiffFile> = diff_files
        .iter()
        .filter(|df| {
            package_of(dir, &df.old_path).as_deref() == Some(package)
                || package_of(dir, &df.new_path).as_deref() == Some(package)
        })
        .collect();
    let (dependencies, module_conditions) = inputs_changed(dir, base_blobs, &module_files, |df| {
        in_package(&df.old_path) || in_package(&df.new_path)
    });
    // crate root は旧新の manifest の `[lib] path` (無ければ `src/lib.rs`)。束縛の比べ直し (#300) は
    // `src` 相対の規約の経路を使うので、規約の root だけを扱う。module の置き場 (#305) は root から
    // たどるので、旧新で同じ root なら扱う。
    let manifest = manifest_path(package);
    let old_root = library_root_in(package, base_blobs.read(&manifest).as_deref());
    let new_root = library_root_in(
        package,
        super::super::source_pair::load_new_source(dir, &manifest)
            .map(|text| text.to_vec())
            .as_deref(),
    );
    let conventional = library_root(package);
    let dependencies = dependencies
        && old_root.as_deref() == Some(conventional.as_str())
        && new_root.as_deref() == Some(conventional.as_str());
    if !dependencies && !module_conditions {
        return Built::Nothing;
    }
    let root = match (old_root, new_root) {
        (Some(old_root), Some(new_root)) if old_root == new_root => old_root,
        _ if module_conditions => {
            return Built::ModuleGap(
                "the library root could not be read or differs between the versions".to_owned(),
            );
        }
        _ => return Built::Nothing,
    };
    let old_paths: HashSet<String> = diff_files
        .iter()
        .filter(|df| df.old_path != "/dev/null")
        .map(|df| df.old_path.clone())
        .collect();
    let new_paths: HashSet<String> = diff_files
        .iter()
        .filter(|df| df.new_path != "/dev/null")
        .map(|df| df.new_path.clone())
        .collect();
    let changed: Rc<HashSet<String>> = Rc::new(old_paths.union(&new_paths).cloned().collect());
    let worktree = move |path: &str| {
        super::super::source_pair::load_new_source(dir, path).map(|source| source.to_vec())
    };
    let read_old: SourceReader<'a> = {
        let old_paths = old_paths.clone();
        let new_paths = new_paths.clone();
        Box::new(move |path: &str| {
            if old_paths.contains(path) {
                base_blobs.read(path)
            } else if new_paths.contains(path) {
                // 差分で足したファイルは旧い版に無い。
                None
            } else {
                worktree(path)
            }
        })
    };
    let read_new: SourceReader<'a> = Box::new(move |path: &str| {
        if !new_paths.contains(path) && old_paths.contains(path) {
            // 差分で消した・移したファイルは新しい版に無い。
            None
        } else {
            worktree(path)
        }
    });
    let shared = Rc::new(RefCell::new(HashMap::new()));
    let old = CrateVersion::new(root.clone(), read_old, changed.clone(), shared.clone());
    let new = CrateVersion::new(root, read_new, changed, shared);
    // lib が旧新の両方に無ければ扱わない (公開面ごと現れる・消える)。読めない・構文エラーなら、
    // module の置き場の条件を比べられない。
    let (old_root, new_root) = (old.root(), new.root());
    let lib_exists = |version: &CrateVersion<'_>| version.read(&version.root).is_some();
    if !matches!(old_root, Opened::File(_)) || !matches!(new_root, Opened::File(_)) {
        return if module_conditions && lib_exists(&old) && lib_exists(&new) {
            Built::ModuleGap("the crate root does not parse".to_owned())
        } else {
            Built::Nothing
        };
    }
    let analysis = if dependencies {
        let renames: Vec<(String, String)> = rename_pairs
            .iter()
            .filter(|(old_path, new_path)| in_package(old_path) || in_package(new_path))
            .cloned()
            .collect();
        super::dependents::analyze(dir, package, &files, &renames, &old, &new)
    } else {
        super::dependents::Analysis {
            changes: ChangeSet::default(),
            moves: MoveMap::new(),
            dependents: Vec::new(),
            incomplete: None,
            multi_placed: HashSet::new(),
        }
    };
    Built::Pair(Box::new(CratePair {
        old,
        new,
        changes: analysis.changes,
        moves: analysis.moves,
        dependents: analysis.dependents,
        incomplete: analysis.incomplete,
        multi_placed: analysis.multi_placed,
        dependencies,
        module_conditions,
        package: package.to_owned(),
    }))
}

/// 差分のファイル `files` で、resolver が読む束縛 ([`DependencyIndex::fingerprint`]。`dependency` が真の
/// ファイルだけ、Issue #300) と、module の置き場の条件に効く入力
/// ([`DependencyIndex::module_condition_fingerprint`]、Issue #305) が旧新で変わったか。ファイルの追加・
/// 削除・rename と、旧新のどちらかが読めない・構文エラーなら変わったとする。旧新は 1 度ずつ解析する。
fn inputs_changed(
    dir: &str,
    base_blobs: &crate::commands::git_input::GitBlobBatch,
    files: &[&crate::models::impact::DiffFile],
    dependency: impl Fn(&crate::models::impact::DiffFile) -> bool,
) -> (bool, bool) {
    let (mut bindings, mut modules) = (false, false);
    for df in files {
        let follows = dependency(df);
        if bindings && modules || (!follows && modules) {
            continue;
        }
        // 版のファイルの索引 (無ければ Some(None)、読めない・構文エラーなら None)。
        let index = |source: Option<Vec<u8>>, path: &str| match source {
            None => Some(None),
            Some(source) => DependencyIndex::new(&source, path).map(Some),
        };
        let old = if df.old_path == "/dev/null" {
            Some(None)
        } else {
            index(base_blobs.read(&df.old_path), &df.old_path)
        };
        let new = if df.new_path == "/dev/null" {
            Some(None)
        } else {
            index(
                super::super::source_pair::load_new_source(dir, &df.new_path)
                    .map(|source| source.to_vec()),
                &df.new_path,
            )
        };
        // 束縛は、同じパスの旧新の索引があるときだけ比べる (rename・追加・削除は変わったとする)。
        bindings |= follows
            && match (&old, &new) {
                (Some(Some(old)), Some(Some(new))) if df.old_path == df.new_path => {
                    old.fingerprint() != new.fingerprint()
                }
                _ => true,
            };
        // module の置き場の条件に効く入力は、無いファイルを `mod` の宣言の無いファイルとして比べる
        // (宣言の無いファイルの追加・削除・rename では変わらない)。読めないファイルは変わったとする。
        let module_fingerprint = |index: &Option<Option<DependencyIndex>>| {
            index.as_ref().map(|index| {
                index
                    .as_ref()
                    .map(DependencyIndex::module_condition_fingerprint)
                    .unwrap_or_default()
            })
        };
        modules |=
            old.is_none() || new.is_none() || module_fingerprint(&old) != module_fingerprint(&new);
    }
    (bindings, modules)
}

/// package の `Cargo.toml` のパス。
fn manifest_path(package: &str) -> String {
    if package.is_empty() {
        "Cargo.toml".to_owned()
    } else {
        format!("{package}/Cargo.toml")
    }
}

/// package の lib crate の root のパス (`dir` 相対): `Cargo.toml` の中身 `manifest` の `[lib] path` (無い・
/// 指定が無ければ `src/lib.rs`)。読めない・解けない・package の外へ出るパスなら None。
fn library_root_in(package: &str, manifest: Option<&[u8]>) -> Option<String> {
    let relative = match manifest {
        None => "src/lib.rs".to_owned(),
        Some(text) => {
            let text = std::str::from_utf8(text).ok()?;
            let value = toml::from_str::<toml::Value>(text).ok()?;
            match value.get("lib").and_then(|lib| lib.get("path")) {
                None => "src/lib.rs".to_owned(),
                Some(path) => path.as_str()?.to_owned(),
            }
        }
    };
    join_path(package, &relative)
}
