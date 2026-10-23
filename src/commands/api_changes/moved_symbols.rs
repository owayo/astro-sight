//! 自由関数・定数を別のファイルへ移すのと同時に変えた宣言を、移動元の削除と移動先の追加の組にする。

use std::collections::{HashMap, HashSet};

use std::rc::Rc;

use tree_sitter::{Node, Point};

use super::moved_members::{MemberFileFacts, Side, Symbols, rust_cfg_applies};
use super::{ApiRelationKey, ApiSymbolCandidate, api_relation_key, bare_name, load_new_source};
use crate::commands::git_input::GitBlobBatch;
use crate::engine::parser;
use crate::engine::rust_signature::{MovedRustConstAdditions, moved_constant_type_resolves_alike};
use crate::language::LangId;
use crate::models::review::MovedSymbol;

/// 組にする宣言 (Rust のトップレベルの関数・定数)。移動先で名前が同じものを指すことを確かめる
/// 検査を Rust にしか持たないので、ほかの言語は組にしない。
fn is_moved_symbol_candidate(path: &str, name: &str, kind: &str) -> bool {
    !name.contains('.')
        && matches!(kind, "function" | "constant")
        && LangId::from_path(camino::Utf8Path::new(path)).ok() == Some(LangId::Rust)
}

impl MemberFileFacts<'_> {
    /// 組になり得るトップレベルの宣言 (ある旧ファイルと別の新ファイルに同じ名前・種別があり、
    /// シグネチャが違う) の名前を参照検索の対象に足す。移さないときと同じく、移動先のファイルの
    /// 中から呼ばれていても参照を数えられるようにする。
    pub(crate) fn collect_moved_symbol_index_names(&self, index_names: &mut HashSet<String>) {
        let mut old_symbols: HashMap<(&str, &str), Vec<(&str, &str)>> = HashMap::new();
        for (path, syms) in &self.old {
            for (name, kind, sig) in syms.iter() {
                if is_moved_symbol_candidate(path, name, kind) {
                    old_symbols
                        .entry((name.as_str(), kind.as_str()))
                        .or_default()
                        .push((*path, sig.as_str()));
                }
            }
        }
        if old_symbols.is_empty() {
            return;
        }
        for (path, (syms, _)) in &self.new {
            for (name, kind, sig) in syms.iter() {
                if is_moved_symbol_candidate(path, name, kind)
                    && old_symbols
                        .get(&(name.as_str(), kind.as_str()))
                        .is_some_and(|olds| {
                            olds.iter().any(|(old_path, old_sig)| {
                                old_path != path && *old_sig != sig.as_str()
                            })
                        })
                {
                    index_names.insert(name.clone());
                }
            }
        }
    }
}

/// 自由関数・定数の移動元の削除と、移動先の追加の組 1 つ。
pub(crate) struct MovedSymbolChange<'a> {
    /// 移動元の宣言 (削除候補)。
    pub(crate) old: ApiSymbolCandidate,
    /// 移動先の宣言 (追加候補)。
    pub(crate) new: ApiSymbolCandidate,
    /// 移動先ファイルの中の呼び出し先 (Phase 0 の抽出結果)。
    pub(crate) new_in_file_callees: &'a HashSet<String>,
    /// 移動元のファイルが変更後も残るなら、その新パス。
    pub(crate) old_file_after: Option<&'a str>,
    /// 移動先のファイルが変更前からあったなら、その旧パス。
    pub(crate) new_file_before: Option<&'a str>,
}

/// 自由関数・定数を別のファイルへ移した同じ差分でシグネチャを変えた、削除と追加の組を返す
/// (削除の並び順)。`removed` / `added` は書き換えない。
///
/// `reconcile_with_moves` は (name, kind, signature) の厳密一致だけを move にするので、移さなければ
/// 互換と判定される変更 (定数の初期化の式、同じ型の別名への書き換え) をした宣言も移動元の削除と
/// 移動先の追加に割れ、移動先の定義と呼び出しが名前の参照に残るため api.rm になる (Issue #114)。
/// 組は呼び出し側が移さないときと同じ規則で判定し、互換と確定したものだけを moved にする。
/// #109 の型のメンバーと違い、自由関数・定数には owner の移動のような対応の証拠が無いので、
/// 組は名前と種別の一意性で決め、構文 ([`proves_moved_declaration`]) と互換の判定で確かめる。
///
/// 組にするのは次をすべて満たすときだけ:
/// - 削除が Rust のトップレベル (名前に `.` が無い) の関数・定数で、同じ (name, kind) の削除が
///   ほかに無い (base と照合できない削除 `unverified_removed`、厳密一致の move に使われた削除も
///   数える)。削除の (name, kind, signature) が対応付け不能 (`ambiguous_relation_keys`) でない
/// - 新規候補に同じ (name, kind) のものが 1 件だけあり (同じファイル・シグネチャの重複は 1 件)、
///   移動元と違う Rust のファイルにあり、シグネチャが違う
/// - 移動元の旧版と移動先の新版のどちらの exported シンボルにも、同じ名前のものが 1 件だけ
///   (変更ファイルで同名が複数あると対応が決まらず modified にしないのと同じ)
pub(crate) fn pair_moved_symbol_changes<'a>(
    removed: &[ApiSymbolCandidate],
    unverified_removed: &[ApiSymbolCandidate],
    all_new_candidates: &[ApiSymbolCandidate],
    moved: &[MovedSymbol],
    ambiguous_relation_keys: &HashSet<ApiRelationKey>,
    files: &MemberFileFacts<'a>,
) -> Vec<MovedSymbolChange<'a>> {
    let mut removals: HashMap<(&str, &str), usize> = HashMap::new();
    for old in removed.iter().chain(unverified_removed) {
        *removals
            .entry((old.name.as_str(), old.kind.as_str()))
            .or_default() += 1;
    }
    for m in moved {
        *removals
            .entry((m.name.as_str(), m.kind.as_str()))
            .or_default() += 1;
    }
    let mut destinations: HashMap<(&str, &str), Vec<usize>> = HashMap::new();
    let mut seen: HashSet<(&str, &str, &str, &str)> = HashSet::new();
    for (new_ix, new) in all_new_candidates.iter().enumerate() {
        if seen.insert((
            new.name.as_str(),
            new.kind.as_str(),
            new.file.as_str(),
            new.signature.as_str(),
        )) {
            destinations
                .entry((new.name.as_str(), new.kind.as_str()))
                .or_default()
                .push(new_ix);
        }
    }
    let declares_once =
        |syms: &Symbols, name: &str| syms.iter().filter(|(n, _, _)| n == name).count() == 1;

    let mut changes = Vec::new();
    for old in removed {
        let key = (old.name.as_str(), old.kind.as_str());
        if !is_moved_symbol_candidate(&old.file, &old.name, &old.kind)
            || removals.get(&key) != Some(&1)
            || ambiguous_relation_keys.contains(&api_relation_key(old))
        {
            continue;
        }
        let Some(&[new_ix]) = destinations.get(&key).map(Vec::as_slice) else {
            continue;
        };
        let new = &all_new_candidates[new_ix];
        if new.file == old.file
            || new.signature == old.signature
            || !is_moved_symbol_candidate(&new.file, &new.name, &new.kind)
        {
            continue;
        }
        let Some(old_syms) = files.old.get(old.file.as_str()) else {
            continue;
        };
        let Some(&(new_syms, new_in_file_callees)) = files.new.get(new.file.as_str()) else {
            continue;
        };
        if declares_once(old_syms, &old.name) && declares_once(new_syms, &new.name) {
            changes.push(MovedSymbolChange {
                old: old.clone(),
                new: new.clone(),
                new_in_file_callees,
                old_file_after: files.new_path_of.get(old.file.as_str()).copied(),
                new_file_before: files.old_path_of.get(new.file.as_str()).copied(),
            });
        }
    }
    changes
}

/// 組の検査で読む Rust ファイル 1 つの解析結果。
struct ParsedRustFile {
    source: Vec<u8>,
    tree: tree_sitter::Tree,
    /// bare 名 → 宣言の範囲 (非公開・入れ子のモジュールの中・型のメンバーも含む)。
    declarations: HashMap<String, Vec<(Point, Point)>>,
}

impl ParsedRustFile {
    /// 解析エラー (ERROR・MISSING) があれば None。
    fn parse(source: Vec<u8>) -> Option<Self> {
        let tree = parser::parse_source(&source, LangId::Rust).ok()?;
        let root = tree.root_node();
        if !super::rust_ref_sites::clean_tree(root) {
            return None;
        }
        let symbols = crate::engine::symbols::extract_symbols(root, &source, LangId::Rust).ok()?;
        let qualnames =
            super::unfiltered_declaration_signatures(&symbols, root, &source, LangId::Rust);
        let mut declarations: HashMap<String, Vec<(Point, Point)>> = HashMap::new();
        for (symbol, (qualname, _, _)) in symbols.iter().zip(&qualnames) {
            declarations
                .entry(bare_name(qualname).to_owned())
                .or_default()
                .push((
                    Point::new(symbol.range.start.line, symbol.range.start.column),
                    Point::new(symbol.range.end.line, symbol.range.end.column),
                ));
        }
        Some(Self {
            source,
            tree,
            declarations,
        })
    }

    fn declarations_of(&self, name: &str) -> &[(Point, Point)] {
        self.declarations.get(name).map_or(&[], Vec::as_slice)
    }

    /// root 直下で外部ファイルを読み込む `mod <name>;` 宣言。
    fn module_declaration(&self, name: &str) -> Option<Node<'_>> {
        let root = self.tree.root_node();
        let mut cursor = root.walk();
        root.named_children(&mut cursor).find(|item| {
            item.kind() == "mod_item"
                && item.child_by_field_name("body").is_none()
                && item
                    .child_by_field_name("name")
                    .and_then(|item_name| item_name.utf8_text(&self.source).ok())
                    == Some(name)
        })
    }
}

/// 読んだファイルの状態。
#[derive(Clone)]
enum LoadedRustFile {
    /// ファイルが無い。
    Missing,
    /// 読めたが解析エラーがある。
    Broken,
    Parsed(Rc<ParsedRustFile>),
}

impl LoadedRustFile {
    fn parsed(self) -> Option<Rc<ParsedRustFile>> {
        match self {
            Self::Parsed(file) => Some(file),
            Self::Missing | Self::Broken => None,
        }
    }
}

/// 自由関数・定数の移動の組を構文で確かめる。読むファイルの解析結果は (側, パス) ごとに 1 回
/// だけ作る (同じファイルの間で複数の宣言を移しても読み直さない)。移動元の旧版と移動先のファイル
/// の旧版は base から、移動先の新版と移動元のファイルの新版は作業ツリーから読む。
pub(crate) struct MovedSymbolFiles<'b> {
    /// ファイルの中身。無ければ None。
    read: ReadSource<'b>,
    files: HashMap<(Side, String), LoadedRustFile>,
    module_chains: HashMap<(Side, String), bool>,
    module_roots: HashMap<(Side, String), String>,
    const_additions: HashMap<(String, String), Option<MovedRustConstAdditions>>,
    project: Option<(&'b str, &'b GitBlobBatch)>,
    project_safe: Option<bool>,
}

type ReadSource<'b> = Box<dyn Fn(Side, &str) -> Option<Vec<u8>> + 'b>;

impl<'b> MovedSymbolFiles<'b> {
    pub(crate) fn new(dir: &'b str, base_blobs: &'b GitBlobBatch) -> Self {
        let mut files = Self::with_reader(move |side, path| match side {
            Side::Old => base_blobs.read(path),
            Side::New => load_new_source(dir, path).map(|source| source.to_vec()),
        });
        files.project = Some((dir, base_blobs));
        files
    }

    fn with_reader(read: impl Fn(Side, &str) -> Option<Vec<u8>> + 'b) -> Self {
        Self {
            read: Box::new(read),
            files: HashMap::new(),
            module_chains: HashMap::new(),
            module_roots: HashMap::new(),
            const_additions: HashMap::new(),
            project: None,
            project_safe: None,
        }
    }

    fn load(&mut self, side: Side, path: &str) -> LoadedRustFile {
        let key = (side, path.to_owned());
        if let Some(loaded) = self.files.get(&key) {
            return loaded.clone();
        }
        let loaded = match (self.read)(side, path) {
            None => LoadedRustFile::Missing,
            Some(source) => match ParsedRustFile::parse(source) {
                Some(file) => LoadedRustFile::Parsed(Rc::new(file)),
                None => LoadedRustFile::Broken,
            },
        };
        self.files.insert(key, loaded.clone());
        loaded
    }

    /// 組の宣言を構文で確かめる (Rust)。読めないファイル・解析エラーのあるファイルがあれば
    /// 証明しない。
    /// - 移動元の旧版と移動先の新版のどちらでも、全宣言 (非公開・入れ子のモジュールの中・型の
    ///   メンバーも数える) で bare 名が一致するものがちょうど 1 件で、それが root の関数・定数・
    ///   static で、宣言とファイルに `cfg` / `cfg_attr` が無い
    /// - crate root から移動元・移動先のファイルまでの `mod` 宣言に `cfg` / `cfg_attr` が無い
    ///   ([`Self::module_chain_without_cfg`])
    /// - 移動元のファイルが変更後も残るなら同じ bare 名の宣言が残っておらず、移動先のファイルが
    ///   変更前からあったなら同じ bare 名の宣言が無かった (公開範囲を狭めて残した・既存の非公開の
    ///   宣言を公開しただけの変更を移動と見なさない)。`use` で名前を import し直すのは宣言に
    ///   数えない (移した先から import し直すのはよくある形)
    pub(crate) fn proves_declaration(&mut self, change: &MovedSymbolChange<'_>) -> bool {
        let name = change.old.name.as_str();
        let root_item = |file: &ParsedRustFile| {
            let [(start, end)] = file.declarations_of(name) else {
                return false;
            };
            let root = file.tree.root_node();
            root.descendant_for_point_range(*start, *end)
                .is_some_and(|node| {
                    matches!(node.kind(), "function_item" | "const_item" | "static_item")
                        && node.parent().is_some_and(|parent| parent.id() == root.id())
                        && !rust_cfg_applies(node, &file.source)
                })
        };
        let undeclared = |loaded: LoadedRustFile| {
            loaded
                .parsed()
                .is_some_and(|file| file.declarations_of(name).is_empty())
        };
        let (Some(old), Some(new)) = (
            self.load(Side::Old, &change.old.file).parsed(),
            self.load(Side::New, &change.new.file).parsed(),
        ) else {
            return false;
        };
        if !root_item(&old) || !root_item(&new) {
            return false;
        }
        if let Some(path) = change.old_file_after
            && !undeclared(self.load(Side::New, path))
        {
            return false;
        }
        if let Some(path) = change.new_file_before
            && !undeclared(self.load(Side::Old, path))
        {
            return false;
        }
        self.module_chain_without_cfg(Side::Old, &change.old.file)
            && self.module_chain_without_cfg(Side::New, &change.new.file)
    }

    /// 値だけの変更の組で、定数の型に現れる名前が移動元と移動先で同じものを指すか
    /// ([`moved_constant_type_resolves_alike`])。
    pub(crate) fn constant_type_resolves_alike(&mut self, change: &MovedSymbolChange<'_>) -> bool {
        let (Some(old), Some(new)) = (
            self.load(Side::Old, &change.old.file).parsed(),
            self.load(Side::New, &change.new.file).parsed(),
        ) else {
            return false;
        };
        moved_constant_type_resolves_alike(
            (old.tree.root_node(), &old.source),
            (new.tree.root_node(), &new.source),
            &change.old.name,
        )
    }

    pub(crate) fn const_addition_is_compatible(&mut self, change: &MovedSymbolChange<'_>) -> bool {
        if change.old.kind != "function" || change.new.kind != "function" {
            return false;
        }
        let (Some(old), Some(new)) = (
            self.load(Side::Old, &change.old.file).parsed(),
            self.load(Side::New, &change.new.file).parsed(),
        ) else {
            return false;
        };
        self.const_additions
            .entry((change.old.file.clone(), change.new.file.clone()))
            .or_insert_with(|| {
                MovedRustConstAdditions::new(
                    (old.tree.root_node(), &old.source),
                    (new.tree.root_node(), &new.source),
                )
            })
            .as_ref()
            .is_some_and(|context| context.proves(&change.new.name))
    }

    /// 現在の各参照が移動先の直接パスを指すことを確かめる。旧パスの更新漏れを、互換な宣言の
    /// 変更だけで moved にしない。import・別名・再公開を経由する参照は証明しない。
    pub(crate) fn references_reach_destination(
        &mut self,
        change: &MovedSymbolChange<'_>,
        index: &super::ApiRefIndex,
    ) -> bool {
        if !self.project_is_safe() {
            return false;
        }
        let Some(refs) = index.complete_refs_for(&change.new.name) else {
            return false;
        };
        let Some((destination, modules)) = cargo_source_module(&change.new.file) else {
            return false;
        };
        let Some((origin, _)) = cargo_source_module(&change.old.file) else {
            return false;
        };
        if self.manifest(Side::Old, origin).is_none() {
            return false;
        }
        let Some(target) = self.manifest(Side::New, destination) else {
            return false;
        };
        if !self.module_chain_without_cfg(Side::New, &change.new.file) {
            return false;
        }
        if !self
            .module_roots
            .get(&(Side::New, change.new.file.clone()))
            .is_some_and(|root| root.ends_with("/src/lib.rs") || root == "src/lib.rs")
        {
            return false;
        }
        let Some(target_name) = target
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(toml::Value::as_str)
        else {
            return false;
        };
        for reference in refs {
            if reference.kind == Some(crate::models::reference::RefKind::Definition) {
                if reference.path != change.new.file {
                    return false;
                }
                continue;
            }
            let Some(file) = self.load(Side::New, &reference.path).parsed() else {
                return false;
            };
            if !source_environment_safe(&file)
                || !self.module_chain_without_cfg(Side::New, &reference.path)
            {
                return false;
            }
            let point = Point::new(reference.line, reference.column);
            let Some(node) = file
                .tree
                .root_node()
                .descendant_for_point_range(point, point)
            else {
                return false;
            };
            if node.utf8_text(&file.source).ok() != Some(&change.new.name) {
                return false;
            }
            if rust_cfg_applies(node, &file.source) {
                return false;
            }
            let Some(path) = direct_reference_path(node, &file.source) else {
                return false;
            };
            // 移動先ファイル内の素の参照は、そのファイルの一意な宣言に結び付く。
            if path == [change.new.name.as_str()] && reference.path == change.new.file {
                continue;
            }
            let Some((caller, _)) = cargo_source_module(&reference.path) else {
                return false;
            };
            let root = if caller == destination {
                if self.module_roots.get(&(Side::New, reference.path.clone()))
                    != self.module_roots.get(&(Side::New, change.new.file.clone()))
                {
                    return false;
                }
                "crate".to_owned()
            } else {
                let Some(manifest) = self.manifest(Side::New, caller) else {
                    return false;
                };
                let Some(dependencies) =
                    manifest.get("dependencies").and_then(toml::Value::as_table)
                else {
                    return false;
                };
                let mut names = dependencies.iter().filter_map(|(name, dependency)| {
                    let dependency = dependency.as_table()?;
                    if dependency
                        .get("optional")
                        .is_some_and(|optional| optional.as_bool() != Some(false))
                    {
                        return None;
                    }
                    let package = dependency
                        .get("package")
                        .and_then(toml::Value::as_str)
                        .unwrap_or(name);
                    let path = dependency.get("path")?.as_str()?;
                    (package == target_name
                        && normalize_dependency_path(caller, path).as_deref() == Some(destination))
                    .then(|| name.replace('-', "_"))
                });
                let (Some(name), None) = (names.next(), names.next()) else {
                    return false;
                };
                name
            };
            let expected: Vec<&str> = std::iter::once(root.as_str())
                .chain(modules.iter().map(String::as_str))
                .chain(std::iter::once(change.new.name.as_str()))
                .collect();
            if path != expected || !path_root_is_unshadowed(&file, &root) {
                return false;
            }
        }
        true
    }

    fn manifest(&self, side: Side, package_dir: &str) -> Option<toml::Value> {
        let source = (self.read)(side, &format!("{package_dir}Cargo.toml"))?;
        let text = std::str::from_utf8(&source).ok()?;
        let manifest: toml::Value = toml::from_str(text).ok()?;
        let targets = crate::engine::rust_targets::RustTargets::parse(
            text,
            std::path::Path::new(package_dir),
        )?;
        if !targets.roots_complete
            || targets.has_explicit_roots()
            || manifest.get("target").is_some()
            || manifest.get("patch").is_some()
            || manifest.get("replace").is_some()
            || manifest
                .get("lib")
                .and_then(|lib| lib.get("name"))
                .is_some()
            || !matches!(
                manifest.get("package")?.get("edition")?.as_str(),
                Some("2018" | "2021" | "2024")
            )
        {
            return None;
        }
        let mut pending = vec![&manifest];
        while let Some(value) = pending.pop() {
            if let Some(table) = value.as_table() {
                for (key, value) in table {
                    if matches!(
                        key.as_str(),
                        "dependencies"
                            | "dev-dependencies"
                            | "build-dependencies"
                            | "dev_dependencies"
                            | "build_dependencies"
                    ) && value.as_table().is_none_or(|deps| {
                        ["std", "core", "alloc"]
                            .iter()
                            .any(|name| deps.contains_key(*name))
                    }) {
                        return None;
                    }
                    pending.push(value);
                }
            }
        }
        if ["std", "core", "alloc"].contains(&manifest.get("package")?.get("name")?.as_str()?) {
            return None;
        }
        Some(manifest)
    }

    /// 別配置や名前を生成するコードが解析 root 内に無いことを、旧新それぞれ一度だけ調べる。
    /// 上限を超える場合は移動の追加証明を使わない。
    fn project_is_safe(&mut self) -> bool {
        if let Some(safe) = self.project_safe {
            return safe;
        }
        let safe = self.scan_project().unwrap_or(false);
        self.project_safe = Some(safe);
        safe
    }

    fn scan_project(&mut self) -> Option<bool> {
        let (dir, blobs) = self.project?;
        crate::commands::git_input::validate_git_revision(blobs.rev(), "base").ok()?;
        let output = crate::git_support::git_command_for_dir(dir)
            // 作業ツリー側・blob reader と同じ解析 root 相対のパスだけを列挙する。
            .args(["ls-tree", "-r", "-z", blobs.rev(), "--", "."])
            .current_dir(dir)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let mut sources = Vec::new();
        for raw in output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
        {
            if sources.len() > 10_000 {
                return None;
            }
            let (metadata, path) = std::str::from_utf8(raw).ok()?.split_once('\t')?;
            let mut fields = metadata.split_whitespace();
            let mode = fields.next()?;
            let kind = fields.next()?;
            if kind != "blob" || !matches!(mode, "100644" | "100755") {
                return None;
            }
            sources.push((Side::Old, path.to_owned()));
        }
        let mut pending = vec![std::path::PathBuf::from(dir)];
        let mut current_count = 0;
        while let Some(directory) = pending.pop() {
            for entry in std::fs::read_dir(directory).ok()? {
                let entry = entry.ok()?;
                let path = entry.path();
                let kind = entry.file_type().ok()?;
                if matches!(entry.file_name().to_str(), Some(".git" | "target")) {
                    continue;
                }
                if kind.is_symlink() {
                    return None;
                }
                if kind.is_dir() {
                    pending.push(path);
                } else if kind.is_file() {
                    current_count += 1;
                    if current_count > 10_000 {
                        return None;
                    }
                    let relative = path.strip_prefix(dir).ok()?.to_str()?.replace('\\', "/");
                    sources.push((Side::New, relative));
                }
            }
        }
        let mut count = 0;
        let mut bytes = 0;
        for (side, path) in sources {
            if path.rsplit('/').next() == Some("build.rs")
                || path.ends_with(".cargo/config")
                || path.ends_with(".cargo/config.toml")
            {
                return None;
            }
            if !path.to_ascii_lowercase().ends_with(".rs") {
                if path.ends_with("Cargo.toml") {
                    let source = (self.read)(side, &path)?;
                    let text = std::str::from_utf8(&source).ok()?;
                    let manifest: toml::Value = toml::from_str(text).ok()?;
                    let package_dir = std::path::Path::new(&path).parent()?;
                    let targets =
                        crate::engine::rust_targets::RustTargets::parse(text, package_dir)?;
                    if !targets.roots_complete
                        || targets.has_explicit_roots()
                        || manifest.get("patch").is_some()
                        || manifest.get("replace").is_some()
                    {
                        return None;
                    }
                }
                continue;
            }
            let file = self.load(side, &path).parsed()?;
            count += 1;
            bytes += file.source.len();
            if count > 512 || bytes > 8 * 1024 * 1024 || !source_environment_safe(&file) {
                return None;
            }
        }
        Some(true)
    }

    /// crate root (`<crate>/src/lib.rs` か `<crate>/src/main.rs`) から `path` までを外部ファイルの
    /// `mod` 宣言でたどり、どの宣言にも (囲むファイルの `#![cfg]` を含め) `cfg` / `cfg_attr` が
    /// 無いか。宣言の有無は候補のファイルだけでは分からない (`#[cfg(unix)] mod unix;` の中の
    /// 宣言を `#[cfg(windows)] mod windows;` へ移すと、どのプラットフォームでも移動にならない)。
    /// `path` をたどれる crate root が無い、`#[path]` の付いた宣言がある、`<mod>.rs` と
    /// `<mod>/mod.rs` の両方がある、途中のファイルが読めない・解析エラーがあるときは証明しない。
    fn module_chain_without_cfg(&mut self, side: Side, path: &str) -> bool {
        let key = (side, path.to_owned());
        if let Some(&proven) = self.module_chains.get(&key) {
            return proven;
        }
        let proven = self.walk_module_chain(side, path).unwrap_or(false);
        self.module_chains.insert(key, proven);
        proven
    }

    /// [`Self::module_chain_without_cfg`] の本体。証明できないときは `Some(false)` か None。
    fn walk_module_chain(&mut self, side: Side, path: &str) -> Option<bool> {
        // `<crate>/src/<rel>` に分ける (最後の `src` ディレクトリ)。
        let (crate_dir, rel) = match path.rfind("/src/") {
            Some(at) => (&path[..at + 1], &path[at + "/src/".len()..]),
            None => ("", path.strip_prefix("src/")?),
        };
        let segments = super::module_path_segments(std::path::Path::new(rel));
        let mut reached = 0;
        let mut reached_root = None;
        for crate_root in ["lib.rs", "main.rs"] {
            let root_path = format!("{crate_dir}src/{crate_root}");
            let mut current = match self.load(side, &root_path) {
                LoadedRustFile::Missing => continue,
                LoadedRustFile::Broken => return None,
                LoadedRustFile::Parsed(file) => file,
            };
            if !source_environment_safe(&current) {
                return Some(false);
            }
            let mut current_path = root_path.clone();
            let mut module_dir = format!("{crate_dir}src/");
            let mut declared = true;
            for segment in &segments {
                let Some(declaration) = current.module_declaration(segment) else {
                    // この crate root からは `path` に届かない
                    declared = false;
                    break;
                };
                if super::rust_mod_item_has_path_attribute(declaration, &current.source)
                    || rust_cfg_applies(declaration, &current.source)
                {
                    return Some(false);
                }
                let as_file = format!("{module_dir}{segment}.rs");
                let as_module = format!("{module_dir}{segment}/mod.rs");
                let (next_path, next) =
                    match (self.load(side, &as_file), self.load(side, &as_module)) {
                        (LoadedRustFile::Parsed(file), LoadedRustFile::Missing) => (as_file, file),
                        (LoadedRustFile::Missing, LoadedRustFile::Parsed(file)) => {
                            (as_module, file)
                        }
                        (LoadedRustFile::Missing, LoadedRustFile::Missing) => {
                            declared = false;
                            break;
                        }
                        _ => return None,
                    };
                current = next;
                if !source_environment_safe(&current) {
                    return Some(false);
                }
                current_path = next_path;
                module_dir = format!("{module_dir}{segment}/");
            }
            if declared && current_path == path {
                reached += 1;
                reached_root = Some(root_path);
            }
        }
        if reached == 1 {
            self.module_roots
                .insert((side, path.to_owned()), reached_root?);
        }
        Some(reached == 1)
    }
}

fn cargo_source_module(path: &str) -> Option<(&str, Vec<String>)> {
    let (package, relative) = match path.rfind("/src/") {
        Some(at) => (&path[..at + 1], &path[at + 5..]),
        None => ("", path.strip_prefix("src/")?),
    };
    if !path.ends_with(".rs") || relative.starts_with("bin/") {
        return None;
    }
    Some((
        package,
        super::module_path_segments(std::path::Path::new(relative)),
    ))
}

fn normalize_dependency_path(package: &str, dependency: &str) -> Option<String> {
    if dependency.starts_with('/') || dependency.contains(['\\', ':']) {
        return None;
    }
    let joined = format!("{package}{dependency}");
    let mut segments = Vec::new();
    for segment in joined.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            _ => segments.push(segment),
        }
    }
    Some(if segments.is_empty() {
        String::new()
    } else {
        format!("{}/", segments.join("/"))
    })
}

/// 通常の scoped_identifier と、マクロ token_tree 内の `a::b::name` だけを読む。
fn direct_reference_path<'a>(mut node: Node<'_>, source: &'a [u8]) -> Option<Vec<&'a str>> {
    let mut path = vec![node.utf8_text(source).ok()?];
    if node.parent()?.kind() == "token_tree" {
        while node
            .prev_sibling()
            .is_some_and(|previous| previous.kind() == "::")
        {
            node = node.prev_sibling()?.prev_sibling()?;
            if !matches!(node.kind(), "identifier" | "crate" | "self" | "super") {
                return None;
            }
            path.push(node.utf8_text(source).ok()?);
        }
        if node
            .prev_sibling()
            .is_some_and(|previous| matches!(previous.kind(), "." | "::"))
        {
            return None;
        }
    } else {
        while let Some(parent) = node
            .parent()
            .filter(|parent| parent.kind() == "scoped_identifier")
        {
            if parent.child_by_field_name("name")?.id() != node.id() {
                return None;
            }
            let mut prefix = parent.child_by_field_name("path")?;
            loop {
                if prefix.kind() == "scoped_identifier" {
                    path.push(prefix.child_by_field_name("name")?.utf8_text(source).ok()?);
                    prefix = prefix.child_by_field_name("path")?;
                } else {
                    if !matches!(prefix.kind(), "identifier" | "crate" | "self" | "super") {
                        return None;
                    }
                    path.push(prefix.utf8_text(source).ok()?);
                    break;
                }
            }
            node = parent;
        }
        if node.parent().is_some_and(|parent| {
            matches!(
                parent.kind(),
                "use_declaration" | "use_as_clause" | "use_list"
            ) || parent.kind() == "field_expression"
                && parent
                    .child_by_field_name("field")
                    .is_some_and(|field| field.id() == node.id())
        }) {
            return None;
        }
    }
    path.reverse();
    Some(path)
}

fn path_root_is_unshadowed(file: &ParsedRustFile, root: &str) -> bool {
    if root == "crate" {
        return true;
    }
    let mut pending = vec![file.tree.root_node()];
    while let Some(node) = pending.pop() {
        if matches!(node.kind(), "attribute_item" | "inner_attribute_item")
            && crate::engine::impact::rust_std_origin::safe_attribute(node, &file.source).is_none()
        {
            return false;
        }
        if matches!(node.kind(), "identifier" | "type_identifier") {
            let Ok(name) = node.utf8_text(&file.source) else {
                return false;
            };
            let name = name.strip_prefix("r#").unwrap_or(name);
            // NFC 同値の束縛を字面だけで除外しない (K は K と同じ名前になる)。
            if !name.is_ascii()
                || name == root
                    && !(node.parent().is_some_and(|parent| {
                        parent.kind() == "scoped_identifier"
                            && parent
                                .child_by_field_name("path")
                                .is_some_and(|path| path.id() == node.id())
                    }) || (node
                        .parent()
                        .is_some_and(|parent| parent.kind() == "token_tree")
                        && node.next_sibling().is_some_and(|next| next.kind() == "::")))
            {
                return false;
            }
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    true
}

fn source_environment_safe(file: &ParsedRustFile) -> bool {
    let mut pending = vec![file.tree.root_node()];
    while let Some(node) = pending.pop() {
        if crate::engine::rust_dynamic_source::node_includes_external_source(node, &file.source) {
            return false;
        }
        match node.kind() {
            "macro_definition" | "extern_crate_declaration" | "use_wildcard" => return false,
            "mod_item"
                if node.child_by_field_name("body").is_none()
                    && node
                        .parent()
                        .is_none_or(|parent| parent.kind() != "source_file") =>
            {
                return false;
            }
            "attribute_item" | "inner_attribute_item" => {
                if crate::engine::impact::rust_std_origin::safe_attribute(node, &file.source)
                    .is_none_or(|names| !names.is_empty())
                {
                    return false;
                }
            }
            "macro_invocation" => {
                let Some(name) = node
                    .child_by_field_name("macro")
                    .and_then(|name| name.utf8_text(&file.source).ok())
                else {
                    return false;
                };
                if !known_standard_macro(name)
                    || node.parent().is_some_and(|parent| {
                        matches!(parent.kind(), "source_file" | "declaration_list")
                            || parent.kind() == "expression_statement"
                                && parent.parent().is_some_and(|parent| {
                                    matches!(parent.kind(), "source_file" | "declaration_list")
                                })
                    })
                {
                    return false;
                }
            }
            "use_declaration" => {
                let mut names = vec![node];
                while let Some(name) = names.pop() {
                    if name
                        .utf8_text(&file.source)
                        .ok()
                        .is_some_and(known_standard_macro)
                    {
                        return false;
                    }
                    let mut cursor = name.walk();
                    names.extend(name.named_children(&mut cursor));
                }
            }
            "token_tree" => {
                let mut cursor = node.walk();
                if node.children(&mut cursor).any(|token| {
                    matches!(token.kind(), "#" | "mod")
                        || token.kind() == "!"
                            && token
                                .next_sibling()
                                .is_some_and(|next| next.kind() == "token_tree")
                            && token
                                .prev_sibling()
                                .is_some_and(|previous| previous.kind() == "identifier")
                            && token.prev_sibling().is_none_or(|name| {
                                name.utf8_text(&file.source)
                                    .ok()
                                    .is_none_or(|name| !known_standard_macro(name))
                                    || name
                                        .prev_sibling()
                                        .is_some_and(|previous| previous.kind() == "::")
                            })
                }) {
                    return false;
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    true
}

fn known_standard_macro(name: &str) -> bool {
    matches!(
        name,
        "println"
            | "print"
            | "eprintln"
            | "eprint"
            | "format"
            | "format_args"
            | "assert"
            | "assert_eq"
            | "assert_ne"
            | "debug_assert"
            | "debug_assert_eq"
            | "debug_assert_ne"
            | "vec"
            | "panic"
            | "write"
            | "writeln"
            | "matches"
            | "todo"
            | "unreachable"
            | "unimplemented"
    )
}

/// 互換と確定した組 (`accepted`) を `moved` に足し、移動元を `removed` から、移動先を `added`
/// から外す。
pub(crate) fn settle_moved_symbol_changes(
    added: Vec<ApiSymbolCandidate>,
    removed: Vec<ApiSymbolCandidate>,
    mut moved: Vec<MovedSymbol>,
    accepted: &[&MovedSymbolChange<'_>],
) -> (
    Vec<ApiSymbolCandidate>,
    Vec<ApiSymbolCandidate>,
    Vec<MovedSymbol>,
) {
    if accepted.is_empty() {
        return (added, removed, moved);
    }
    let candidate_key = |c: &ApiSymbolCandidate| {
        (
            c.name.clone(),
            c.kind.clone(),
            c.file.clone(),
            c.signature.clone(),
        )
    };
    let paired_old: HashSet<_> = accepted
        .iter()
        .map(|change| candidate_key(&change.old))
        .collect();
    let paired_new: HashSet<_> = accepted
        .iter()
        .map(|change| candidate_key(&change.new))
        .collect();
    moved.extend(accepted.iter().map(|change| MovedSymbol {
        name: change.old.name.clone(),
        kind: change.old.kind.clone(),
        from: change.old.file.clone(),
        to: change.new.file.clone(),
    }));
    let added = added
        .into_iter()
        .filter(|a| !paired_new.contains(&candidate_key(a)))
        .collect();
    let removed = removed
        .into_iter()
        .filter(|r| !paired_old.contains(&candidate_key(r)))
        .collect();
    (added, removed, moved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::api_changes::PreparedDiffFile;
    use crate::models::impact::DiffFile;

    #[test]
    fn moved_manifest_rejects_legacy_sysroot_dependencies() {
        for table in [
            "dependencies",
            "dev-dependencies",
            "build-dependencies",
            "dev_dependencies",
            "build_dependencies",
        ] {
            for name in ["std", "core", "alloc"] {
                for target in ["", "target.'cfg(windows)'."] {
                    let source = format!(
                        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[{target}{table}]\n{name} = {{ package = \"fake_std\", path = \"../fake\" }}\n"
                    );
                    let files =
                        MovedSymbolFiles::with_reader(|_, _| Some(source.as_bytes().to_vec()));
                    for side in [Side::Old, Side::New] {
                        assert!(
                            files.manifest(side, "").is_none(),
                            "{target}{table}: {name}"
                        );
                    }
                }
            }
            let source = format!(
                "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[{table}]\nother = \"1\"\n"
            );
            let files = MovedSymbolFiles::with_reader(|_, _| Some(source.as_bytes().to_vec()));
            assert!(files.manifest(Side::New, "").is_some(), "{table}");
        }
    }

    #[test]
    fn destination_root_proof_rejects_raw_and_nfc_shadowing() {
        let mut accepted = Vec::new();
        for (root, binding) in [("beta", "r#beta"), ("K", "K")] {
            let source = format!(
                "use alpha as {binding};\nfn main() {{ let _: u8 = {root}::items::WEEK; }}"
            );
            let file = ParsedRustFile::parse(source.into_bytes()).unwrap();
            if path_root_is_unshadowed(&file, root) {
                accepted.push(binding);
            }
        }
        assert!(accepted.is_empty(), "{accepted:?}");
        let file = ParsedRustFile::parse(b"fn main() { let _: u8 = beta::items::WEEK; }".to_vec())
            .unwrap();
        assert!(path_root_is_unshadowed(&file, "beta"));
    }

    #[test]
    fn direct_reference_paths_use_real_scoped_and_macro_nodes() {
        for marked in [
            "fn main() { beta::items::@WEEK.len(); }",
            "fn main() { println!(\"{}\", beta::items::@WEEK.len()); }",
        ] {
            let offset = marked.find('@').unwrap();
            let source = marked.replace('@', "");
            let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
            let point = Point::new(0, offset);
            let node = tree
                .root_node()
                .descendant_for_point_range(point, point)
                .unwrap();
            let mut cursor = node.parent().unwrap().walk();
            let siblings: Vec<_> = node
                .parent()
                .unwrap()
                .children(&mut cursor)
                .map(|child| (child.kind(), child.utf8_text(source.as_bytes()).unwrap()))
                .collect();
            assert_eq!(
                direct_reference_path(node, source.as_bytes()),
                Some(vec!["beta", "items", "WEEK"]),
                "{marked}: {siblings:?}"
            );
        }
    }

    const OLD: &str = "a/src/items.rs";
    const NEW: &str = "b/src/items.rs";
    const OLD_SIG: &str = "pub const WEEK: [u8; 2] = [1, 2];";
    const NEW_SIG: &str = "pub const WEEK: [u8; 2] = [ONE, 2];";

    fn symbol(name: &str, kind: &str, signature: &str) -> (String, String, String) {
        (name.to_owned(), kind.to_owned(), signature.to_owned())
    }

    fn candidate(name: &str, kind: &str, file: &str, signature: &str) -> ApiSymbolCandidate {
        ApiSymbolCandidate {
            name: name.to_owned(),
            kind: kind.to_owned(),
            file: file.to_owned(),
            signature: signature.to_owned(),
        }
    }

    fn diff(old_path: &str, new_path: &str) -> DiffFile {
        DiffFile {
            old_path: old_path.to_owned(),
            new_path: new_path.to_owned(),
            hunks: Vec::new(),
            deleted_old_source: None,
        }
    }

    /// `WEEK` を OLD から NEW へ移し、初期化の式を変えた差分の材料。
    struct Scenario {
        old_syms: Vec<(String, String, String)>,
        new_syms: Vec<(String, String, String)>,
        removed: Vec<ApiSymbolCandidate>,
        unverified: Vec<ApiSymbolCandidate>,
        all_new: Vec<ApiSymbolCandidate>,
        moved: Vec<MovedSymbol>,
        ambiguous: HashSet<ApiRelationKey>,
    }

    impl Scenario {
        fn new() -> Self {
            Self {
                old_syms: vec![symbol("WEEK", "constant", OLD_SIG)],
                new_syms: vec![symbol("WEEK", "constant", NEW_SIG)],
                removed: vec![candidate("WEEK", "constant", OLD, OLD_SIG)],
                unverified: Vec::new(),
                all_new: vec![candidate("WEEK", "constant", NEW, NEW_SIG)],
                moved: Vec::new(),
                ambiguous: HashSet::new(),
            }
        }

        /// 組にした (移動元, 移動先, 移動先のシグネチャ)。
        fn pair(&self) -> Vec<(String, String, String)> {
            let diff_files = [diff(OLD, "/dev/null"), diff("/dev/null", NEW)];
            let prepared = [
                PreparedDiffFile::Deleted {
                    old_syms: Some(self.old_syms.clone()),
                    old_source_unverified: false,
                },
                PreparedDiffFile::Added {
                    new_syms: Some(self.new_syms.clone()),
                    in_file_callees: HashSet::new(),
                },
            ];
            let files = MemberFileFacts::new(&diff_files, &prepared);
            pair_moved_symbol_changes(
                &self.removed,
                &self.unverified,
                &self.all_new,
                &self.moved,
                &self.ambiguous,
                &files,
            )
            .into_iter()
            .map(|change| (change.old.file, change.new.file, change.new.signature))
            .collect()
        }
    }

    #[test]
    fn moved_symbol_pairs_a_unique_removal_and_addition() {
        assert_eq!(
            Scenario::new().pair(),
            vec![(OLD.to_owned(), NEW.to_owned(), NEW_SIG.to_owned())]
        );
        // 同じファイル・シグネチャの重複候補は 1 件と数える。
        let mut scenario = Scenario::new();
        scenario.all_new.push(scenario.all_new[0].clone());
        assert_eq!(scenario.pair().len(), 1);
        // 関数も同じ。
        let mut scenario = Scenario::new();
        scenario.old_syms[0].1 = "function".to_owned();
        scenario.new_syms[0].1 = "function".to_owned();
        scenario.removed[0].kind = "function".to_owned();
        scenario.all_new[0].kind = "function".to_owned();
        assert_eq!(scenario.pair().len(), 1);
    }

    /// 移動元のファイルが変更後も残る・移動先のファイルが変更前からあったときは、その版を
    /// 構文の検査に渡せるようパスを添える。
    #[test]
    fn moved_symbol_carries_the_surviving_file_paths() {
        let scenario = Scenario::new();
        let renamed = "a/src/renamed.rs";
        let diff_files = [diff(OLD, renamed), diff(NEW, NEW)];
        let prepared = [
            PreparedDiffFile::Modified {
                unparsed_declarations: Vec::new(),
                old_syms: Some(scenario.old_syms.clone()),
                new_syms: Some(Vec::new()),
                parameter_pattern_only_names: HashSet::new(),
                rust_const_additions: HashSet::new(),
                in_file_callees: HashSet::new(),
                new_export_surface_names: HashSet::new(),
            },
            PreparedDiffFile::Modified {
                unparsed_declarations: Vec::new(),
                old_syms: Some(Vec::new()),
                new_syms: Some(scenario.new_syms.clone()),
                parameter_pattern_only_names: HashSet::new(),
                rust_const_additions: HashSet::new(),
                in_file_callees: HashSet::new(),
                new_export_surface_names: HashSet::new(),
            },
        ];
        let files = MemberFileFacts::new(&diff_files, &prepared);
        let changes = pair_moved_symbol_changes(
            &scenario.removed,
            &[],
            &scenario.all_new,
            &[],
            &HashSet::new(),
            &files,
        );
        assert_eq!(
            changes
                .iter()
                .map(|change| (change.old_file_after, change.new_file_before))
                .collect::<Vec<_>>(),
            [(Some(renamed), Some(NEW))]
        );
    }

    #[test]
    fn moved_symbol_rejects_every_ambiguous_correspondence() {
        type Edit = fn(&mut Scenario);
        let cases: [(&str, Edit); 15] = [
            ("同じ (name, kind) の削除がほかにもある", |s| {
                s.removed
                    .push(candidate("WEEK", "constant", "c/src/items.rs", OLD_SIG));
            }),
            (
                "同じ (name, kind) の base と照合できない削除がある",
                |s| {
                    s.unverified
                        .push(candidate("WEEK", "constant", "c/src/items.rs", OLD_SIG));
                },
            ),
            (
                "同じ (name, kind) の削除が厳密一致で移動している",
                |s| {
                    s.moved.push(MovedSymbol {
                        name: "WEEK".to_owned(),
                        kind: "constant".to_owned(),
                        from: "c/src/items.rs".to_owned(),
                        to: "d/src/items.rs".to_owned(),
                    });
                },
            ),
            ("削除の key が対応付け不能", |s| {
                s.ambiguous.insert(api_relation_key(&s.removed[0]));
            }),
            ("移動先の候補が 2 ファイルにある", |s| {
                s.all_new
                    .push(candidate("WEEK", "constant", "c/src/items.rs", NEW_SIG));
            }),
            (
                "移動先のファイルに同じ (name, kind) の候補が 2 件ある",
                |s| {
                    s.all_new.push(candidate(
                        "WEEK",
                        "constant",
                        NEW,
                        "pub const WEEK: [u8; 2] = [2, 1];",
                    ));
                },
            ),
            (
                "移動先のファイルに同じ名前の宣言が 2 件ある",
                |s| {
                    s.new_syms.push(symbol("WEEK", "function", "pub fn WEEK()"));
                },
            ),
            (
                "移動元のファイルに同じ名前の宣言が 2 件ある",
                |s| {
                    s.old_syms.push(symbol("WEEK", "function", "pub fn WEEK()"));
                },
            ),
            ("シグネチャが同じ", |s| {
                s.all_new[0].signature = OLD_SIG.to_owned();
            }),
            ("移動先が移動元と同じファイル", |s| {
                s.all_new[0].file = OLD.to_owned();
            }),
            ("メンバー (名前に `.` がある)", |s| {
                s.removed[0].name = "Days.WEEK".to_owned();
                s.all_new[0].name = "Days.WEEK".to_owned();
            }),
            ("型の宣言", |s| {
                s.removed[0].kind = "struct".to_owned();
                s.all_new[0].kind = "struct".to_owned();
            }),
            ("変数 (Rust の関数・定数でない種別)", |s| {
                s.removed[0].kind = "variable".to_owned();
                s.all_new[0].kind = "variable".to_owned();
            }),
            ("移動元が Rust のファイルでない", |s| {
                s.removed[0].file = "a/src/items.ts".to_owned();
            }),
            ("移動先が Rust のファイルでない", |s| {
                s.all_new[0].file = "b/src/items.ts".to_owned();
            }),
        ];
        for (label, edit) in cases {
            let mut scenario = Scenario::new();
            edit(&mut scenario);
            assert!(scenario.pair().is_empty(), "{label}");
        }
    }

    /// 組にした削除と追加だけを外し、moved は組の並び順で足す。
    #[test]
    fn settle_moved_symbol_changes_removes_only_accepted_pairs() {
        let callees = HashSet::new();
        let change = |name: &str| MovedSymbolChange {
            old: candidate(name, "constant", OLD, OLD_SIG),
            new: candidate(name, "constant", NEW, NEW_SIG),
            new_in_file_callees: &callees,
            old_file_after: None,
            new_file_before: None,
        };
        let (week, day) = (change("WEEK"), change("DAY"));
        let (added, removed, moved) = settle_moved_symbol_changes(
            vec![
                candidate("WEEK", "constant", NEW, NEW_SIG),
                candidate("KEEP", "constant", NEW, NEW_SIG),
            ],
            vec![
                candidate("DAY", "constant", OLD, OLD_SIG),
                candidate("WEEK", "constant", OLD, OLD_SIG),
                candidate("GONE", "constant", OLD, OLD_SIG),
            ],
            Vec::new(),
            &[&day, &week],
        );
        let names = |candidates: &[ApiSymbolCandidate]| {
            candidates
                .iter()
                .map(|c| c.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&added), ["KEEP"]);
        assert_eq!(names(&removed), ["GONE"]);
        assert_eq!(
            moved
                .iter()
                .map(|m| (m.name.as_str(), m.from.as_str(), m.to.as_str()))
                .collect::<Vec<_>>(),
            [("DAY", OLD, NEW), ("WEEK", OLD, NEW)]
        );
    }

    /// 構文の検査の材料。`a` クレートの `items.rs` の `WEEK` を `b` クレートの `items.rs` へ移す。
    struct Layout {
        /// (側, パス, 中身)。旧側は base、新側は作業ツリー。
        files: Vec<(Side, String, String)>,
        old_file: String,
        new_file: String,
        old_file_after: Option<String>,
        new_file_before: Option<String>,
    }

    impl Layout {
        fn new() -> Self {
            let file =
                |side, path: &str, content: &str| (side, path.to_owned(), content.to_owned());
            Self {
                files: vec![
                    file(Side::Old, "a/src/lib.rs", "pub mod items;\n"),
                    file(
                        Side::Old,
                        "a/src/items.rs",
                        "pub const WEEK: u8 = 1;\npub fn other() {}\n",
                    ),
                    file(Side::New, "b/src/lib.rs", "pub mod items;\n"),
                    file(
                        Side::New,
                        "b/src/items.rs",
                        "use std::fmt;\n\npub const WEEK: u8 = 2;\n",
                    ),
                ],
                old_file: "a/src/items.rs".to_owned(),
                new_file: "b/src/items.rs".to_owned(),
                old_file_after: None,
                new_file_before: None,
            }
        }

        /// `side` の `path` を `content` にする (無ければ足す)。
        fn set(&mut self, side: Side, path: &str, content: &str) -> &mut Self {
            self.files.retain(|(s, p, _)| !(*s == side && p == path));
            self.files.push((side, path.to_owned(), content.to_owned()));
            self
        }

        fn remove(&mut self, side: Side, path: &str) -> &mut Self {
            self.files.retain(|(s, p, _)| !(*s == side && p == path));
            self
        }

        fn proves(&self) -> bool {
            let callees = HashSet::new();
            let change = MovedSymbolChange {
                old: candidate("WEEK", "constant", &self.old_file, OLD_SIG),
                new: candidate("WEEK", "constant", &self.new_file, NEW_SIG),
                new_in_file_callees: &callees,
                old_file_after: self.old_file_after.as_deref(),
                new_file_before: self.new_file_before.as_deref(),
            };
            let mut files = MovedSymbolFiles::with_reader(|side, path| {
                self.files
                    .iter()
                    .find(|(s, p, _)| *s == side && p == path)
                    .map(|(_, _, content)| content.as_bytes().to_vec())
            });
            files.proves_declaration(&change)
        }
    }

    /// 宣言の旧新・移動元に残る版・移動先の元の版・crate root からの `mod` の鎖を構文で確かめる。
    #[test]
    fn moved_declaration_requires_a_unique_root_item_that_really_moved() {
        type Edit = fn(&mut Layout);
        let accepted: [(&str, Edit); 7] = [
            ("基本", |_| {}),
            ("移した先から import し直す", |l| {
                l.old_file_after = Some("a/src/items.rs".to_owned());
                l.set(
                    Side::New,
                    "a/src/items.rs",
                    "use b::items::WEEK;\npub fn other() {}\n",
                );
            }),
            ("移動先に別の宣言が元からある", |l| {
                l.new_file_before = Some("b/src/items.rs".to_owned());
                l.set(Side::Old, "b/src/items.rs", "pub fn unrelated() {}\n");
            }),
            ("関数", |l| {
                l.set(Side::Old, "a/src/items.rs", "pub fn WEEK() {}\n");
                l.set(Side::New, "b/src/items.rs", "pub fn WEEK(x: u8) {}\n");
            }),
            (
                "入れ子のモジュールのファイル (mod.rs も)",
                |l| {
                    l.set(Side::Old, "a/src/lib.rs", "pub mod outer;\n");
                    l.set(Side::Old, "a/src/outer.rs", "pub mod items;\n");
                    l.set(
                        Side::Old,
                        "a/src/outer/items.rs",
                        "pub const WEEK: u8 = 1;\n",
                    );
                    l.old_file = "a/src/outer/items.rs".to_owned();
                    l.set(Side::New, "b/src/items/mod.rs", "pub const WEEK: u8 = 2;\n");
                    l.remove(Side::New, "b/src/items.rs");
                    l.new_file = "b/src/items/mod.rs".to_owned();
                },
            ),
            ("main.rs から届くファイル", |l| {
                l.remove(Side::New, "b/src/lib.rs");
                l.set(Side::New, "b/src/main.rs", "mod items;\nfn main() {}\n");
            }),
            ("crate root そのもの", |l| {
                l.set(Side::New, "b/src/lib.rs", "pub const WEEK: u8 = 2;\n");
                l.new_file = "b/src/lib.rs".to_owned();
            }),
        ];
        for (label, edit) in accepted {
            let mut layout = Layout::new();
            edit(&mut layout);
            assert!(layout.proves(), "{label}");
        }
        let rejected: [(&str, Edit); 19] = [
            ("移動元に同じ bare 名の宣言が 2 件", |l| {
                l.set(
                    Side::Old,
                    "a/src/items.rs",
                    "pub const WEEK: u8 = 1;\nmod inner {\n    const WEEK: u8 = 3;\n}\n",
                );
            }),
            ("移動先に同じ bare 名のメソッドがある", |l| {
                l.set(
                    Side::New,
                    "b/src/items.rs",
                    "pub const WEEK: u8 = 2;\npub struct S;\nimpl S {\n    pub fn WEEK(&self) {}\n}\n",
                );
            }),
            (
                "移動先で入れ子のモジュールの中にある",
                |l| {
                    l.set(
                        Side::New,
                        "b/src/items.rs",
                        "pub mod inner {\n    pub const WEEK: u8 = 2;\n}\n",
                    );
                },
            ),
            ("移動先の宣言に cfg", |l| {
                l.set(
                    Side::New,
                    "b/src/items.rs",
                    "#[cfg(unix)]\npub const WEEK: u8 = 2;\n",
                );
            }),
            ("移動元のファイルに cfg", |l| {
                l.set(
                    Side::Old,
                    "a/src/items.rs",
                    "#![cfg(unix)]\npub const WEEK: u8 = 1;\n",
                );
            }),
            ("移動元の mod 宣言に cfg", |l| {
                l.set(Side::Old, "a/src/lib.rs", "#[cfg(unix)]\npub mod items;\n");
            }),
            ("移動先の mod 宣言に cfg_attr", |l| {
                l.set(
                    Side::New,
                    "b/src/lib.rs",
                    "#[cfg_attr(unix, allow(dead_code))]\npub mod items;\n",
                );
            }),
            ("crate root に cfg", |l| {
                l.set(Side::New, "b/src/lib.rs", "#![cfg(unix)]\npub mod items;\n");
            }),
            ("mod 宣言に path 属性", |l| {
                l.set(
                    Side::Old,
                    "a/src/lib.rs",
                    "#[path = \"items.rs\"]\npub mod items;\n",
                );
            }),
            ("items.rs と items/mod.rs の両方がある", |l| {
                l.set(
                    Side::New,
                    "b/src/items/mod.rs",
                    "pub const OTHER: u8 = 3;\n",
                );
            }),
            ("crate root から届かない", |l| {
                l.set(Side::New, "b/src/lib.rs", "pub mod other;\n");
            }),
            ("crate root が無い", |l| {
                l.remove(Side::Old, "a/src/lib.rs");
            }),
            ("src の下にない", |l| {
                l.set(Side::Old, "a/items.rs", "pub const WEEK: u8 = 1;\n");
                l.old_file = "a/items.rs".to_owned();
            }),
            ("移動元に公開範囲を狭めて残した", |l| {
                l.old_file_after = Some("a/src/items.rs".to_owned());
                l.set(
                    Side::New,
                    "a/src/items.rs",
                    "pub(crate) const WEEK: u8 = 1;\n",
                );
            }),
            (
                "移動先にあった非公開の宣言を公開した",
                |l| {
                    l.new_file_before = Some("b/src/items.rs".to_owned());
                    l.set(Side::Old, "b/src/items.rs", "const WEEK: u8 = 2;\n");
                },
            ),
            ("移動元の残りのファイルが読めない", |l| {
                l.old_file_after = Some("a/src/renamed.rs".to_owned());
            }),
            ("移動先の元の版に解析エラー", |l| {
                l.new_file_before = Some("b/src/items.rs".to_owned());
                l.set(Side::Old, "b/src/items.rs", "pub fn broken( {\n");
            }),
            ("移動先に解析エラー", |l| {
                l.set(Side::New, "b/src/items.rs", "pub const WEEK: u8 = ;\n");
            }),
            ("crate root に解析エラー", |l| {
                l.set(Side::Old, "a/src/lib.rs", "pub mod items\n");
            }),
        ];
        for (label, edit) in rejected {
            let mut layout = Layout::new();
            edit(&mut layout);
            assert!(!layout.proves(), "{label}");
        }
    }

    /// 組になり得るトップレベルの宣言だけを参照検索の対象に足す。
    #[test]
    fn moved_symbol_index_names_cover_only_possible_pairs() {
        let diff_files = [
            diff(OLD, "/dev/null"),
            diff("/dev/null", NEW),
            diff("c/src/same.rs", "c/src/same.rs"),
        ];
        let prepared = [
            PreparedDiffFile::Deleted {
                old_syms: Some(vec![
                    symbol("WEEK", "constant", OLD_SIG),
                    symbol("keep", "function", "pub fn keep()"),
                    symbol("Packer.pack", "function", "pub fn pack(&self)"),
                    symbol("Packer", "struct", "pub struct Packer"),
                ]),
                old_source_unverified: false,
            },
            PreparedDiffFile::Added {
                new_syms: Some(vec![
                    symbol("WEEK", "constant", NEW_SIG),
                    symbol("keep", "function", "pub fn keep()"),
                    symbol("Packer.pack", "function", "pub fn pack(&self, n: u32)"),
                    symbol("Packer", "struct", "pub struct Packer<T>"),
                ]),
                in_file_callees: HashSet::new(),
            },
            PreparedDiffFile::Modified {
                unparsed_declarations: Vec::new(),
                old_syms: Some(vec![
                    symbol("local", "function", "pub fn local()"),
                    symbol("week", "variable", "export const week = 1;"),
                ]),
                new_syms: Some(vec![symbol("local", "function", "pub fn local(n: u32)")]),
                parameter_pattern_only_names: HashSet::new(),
                rust_const_additions: HashSet::new(),
                in_file_callees: HashSet::new(),
                new_export_surface_names: HashSet::new(),
            },
        ];
        let mut names = HashSet::new();
        MemberFileFacts::new(&diff_files, &prepared).collect_moved_symbol_index_names(&mut names);
        assert_eq!(names, HashSet::from(["WEEK".to_owned()]));
    }
}
