//! 末尾の省略可能引数・省略可能 props の削除と、リポジトリ内の利用の証拠。

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use tree_sitter::Node;

use super::outside::{OutsideScan, module_word};
use super::repository_audit::{
    PackageBoundary, RepositoryAudit, RepositoryFiles, Settings, declares_jsx_name,
    replaces_jsx_environment,
};
use super::source_scan::{Budget, ScanLimits};
use super::*;
use crate::commands::api_changes::ts_const_arg::collect_bindings_named;
use crate::commands::dead_code_default_liveness::{resolve_relative_specifier, static_specifier};
use crate::engine::parser::SourceBuf;
use crate::engine::refs::{FileScanOptions, collect_files_scan};
use crate::git_support::normalize_workspace_separators;
use crate::language::LangId;
use crate::models::reference::SymbolReference;

const MAX_FILES: usize = 512;
const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_NODES: usize = 100_000;
const MAX_DEPTH: usize = 128;

pub(super) struct RemovalFile {
    source: SourceBuf,
    tree: tree_sitter::Tree,
    lang: LangId,
}

/// 基本の集合の外から在庫に加えたファイル (`dir` 相対のパス → 解析した中身)。
type OutsideUses = BTreeMap<String, Rc<RemovalFile>>;

/// 解析した外のファイルと、その構文木のノード数。
type ParsedOutside = (Rc<RemovalFile>, usize);

/// 在庫に入れたファイルの数・大きさ・構文木のノード数。上限は在庫に入れたファイルにだけ掛ける。
#[derive(Clone, Copy, Default)]
struct InventoryTotals {
    files: usize,
    bytes: usize,
    nodes: usize,
}

impl InventoryTotals {
    /// 1 ファイルを足す。上限を超えたら None。
    fn add(mut self, bytes: usize, nodes: usize) -> Option<Self> {
        self.files += 1;
        self.bytes = self.bytes.checked_add(bytes)?;
        self.nodes = self.nodes.checked_add(nodes)?;
        (self.files <= MAX_FILES && self.bytes <= MAX_BYTES && self.nodes <= MAX_NODES)
            .then_some(self)
    }
}

/// 在庫の基本の集合 (ignore と隠しファイルの除外に従う集合) を、全部読んで解析したもの。
pub(super) struct RemovalInventory {
    files: BTreeMap<String, RemovalFile>,
    totals: InventoryTotals,
}

impl RemovalInventory {
    /// 在庫の各ファイルの `dir` 相対パス・内容・構文木の根・言語 (パスの昇順)。
    pub(super) fn parsed(&self) -> impl Iterator<Item = (&str, &[u8], Node<'_>, LangId)> {
        self.files.iter().map(|(path, file)| {
            (
                path.as_str(),
                file.source.as_ref(),
                file.tree.root_node(),
                file.lang,
            )
        })
    }
}

/// 読んで解析した 1 ファイル。TS / JS 系でなければ None。
fn read_source(path: &Path) -> Option<(RemovalFile, usize)> {
    let utf8 = camino::Utf8Path::from_path(path)?;
    let source = parser::read_file(utf8).ok()?;
    let (tree, lang) = parser::parse_file(utf8, &source).ok()?;
    // 他言語の依存や埋め込み構文は、この閉包証明では解決しない。
    if !matches!(lang, LangId::Typescript | LangId::Tsx | LangId::Javascript) {
        return None;
    }
    let nodes = checked_nodes(tree.root_node())?.len();
    Some((RemovalFile { source, tree, lang }, nodes))
}

/// 基本の集合を、上限の中で全部読んで解析する (この PR の前の在庫と同じ規則)。
fn load_base_inventory(root: &Path, paths: &[PathBuf]) -> Option<RemovalInventory> {
    let mut files = BTreeMap::new();
    let mut totals = InventoryTotals::default();
    for path in paths {
        let path = path.canonicalize().ok()?;
        let relative =
            normalize_workspace_separators(&path.strip_prefix(root).ok()?.to_string_lossy());
        let (file, nodes) = read_source(&path)?;
        totals = totals.add(file.source.len(), nodes)?;
        files.insert(relative, file);
    }
    Some(RemovalInventory { files, totals })
}

/// 利用の在庫の基本の集合、その外のファイルを 1 回読んだ結果、設定と package.json、監査。
pub(super) struct RemovalEvidence {
    pub(super) base: RemovalInventory,
    /// 在庫の外のファイル。読めない・上限を超えたら None (どの証明もしない)。
    outside: Option<OutsideScan>,
    /// 設定と package.json。読めなければ None (外の bare import の解決先を決められない)。
    settings: Option<Settings>,
    /// 監査が成り立たない (先を監査できない symlink、外を取り込む設定など) ときは None。
    pub(super) repository: Option<RepositoryAudit>,
    /// 読む量と解析する量の残り (1 回の検出の全体で共有する)。
    budget: Budget,
    /// 在庫に加えるために読んで解析した外のファイルと、その構文木のノード数 (パスごとに、
    /// 検出の全体で共有する)。None は読めなかった・上限を超えた。
    parsed: RefCell<HashMap<String, Option<ParsedOutside>>>,
}

impl RemovalEvidence {
    pub(super) fn load(dir: &str, words: &[String]) -> Option<Self> {
        Self::load_with(dir, words, ScanLimits::DEFAULT)
    }

    /// 基本の集合はこの PR の前の在庫と同じ規則で作る (上限超過・他言語のソース・解析できない
    /// ソースがあれば None)。ignore に関係なく走査できなければ (読めないエントリなど) None。基本の
    /// 集合の外は `words` (候補の関数名と宣言のファイルを指す名前) と依存の印を 1 回だけ照合する。
    pub(super) fn load_with(dir: &str, words: &[String], limits: ScanLimits) -> Option<Self> {
        let root = Path::new(dir).canonicalize().ok()?;
        let collection = collect_files_scan(
            &root,
            None,
            FileScanOptions {
                include_generated: true,
            },
        )
        .ok()?;
        if collection.files.len() > MAX_FILES
            || !collection.unanalyzable_truncations(&root).is_empty()
        {
            return None;
        }
        let base = load_base_inventory(&root, &collection.files)?;
        let in_base: HashSet<PathBuf> = collection
            .files
            .iter()
            .chain(collection.unanalyzable_source_paths())
            .cloned()
            .collect();
        let mut files = RepositoryFiles::walk(dir, &in_base)?;
        let settings = Settings::load(&files);
        let budget = Budget::new(limits);
        let outside = OutsideScan::scan(
            &root,
            std::mem::take(&mut files.outside),
            words.to_vec(),
            settings.as_ref().map(|settings| &settings.packages),
            settings.as_ref().is_some_and(|settings| settings.allow_js),
            &budget,
        );
        let repository = RepositoryAudit::load(
            &files,
            settings.as_ref(),
            &base,
            outside.as_ref().and_then(|outside| outside.jsx_standard),
        );
        Some(Self {
            base,
            outside,
            settings,
            repository,
            budget,
            parsed: RefCell::new(HashMap::new()),
        })
    }

    /// 宣言のファイル `target` にある `name` のために、基本の集合の外から在庫に加えるファイルを読んで
    /// 解析する。選ぶ規則は [`OutsideScan::selected`]。読めない・上限を超えるときは None。
    pub(super) fn outside_uses(&self, name: &str, target: &str) -> Option<OutsideUses> {
        let outside = self.outside.as_ref()?;
        let packages = self.settings.as_ref().map(|settings| &settings.packages);
        let mut totals = self.base.totals;
        let mut extra = BTreeMap::new();
        for source in outside.selected(name, target, packages, &self.budget)? {
            let size = usize::try_from(std::fs::metadata(&source.path).ok()?.len()).ok()?;
            if totals.bytes.checked_add(size)? > MAX_BYTES {
                return None;
            }
            let (file, nodes) = self.parsed_outside(source)?;
            totals = totals.add(file.source.len(), nodes)?;
            extra.insert(source.relative.clone(), file);
        }
        Some(extra)
    }

    /// 外のファイルを読んで解析する。パスごとに 1 度だけ解析し、その量を共有の解析の予算に数える。
    fn parsed_outside(
        &self,
        source: &super::repository_audit::OutsideSource,
    ) -> Option<ParsedOutside> {
        if let Some(parsed) = self.parsed.borrow().get(&source.relative) {
            return parsed.clone();
        }
        let parsed = std::fs::metadata(&source.path)
            .ok()
            .filter(|metadata| self.budget.take_parse(metadata.len()))
            .and_then(|_| read_source(&source.path))
            .map(|(file, nodes)| (Rc::new(file), nodes));
        self.parsed
            .borrow_mut()
            .insert(source.relative.clone(), parsed.clone());
        parsed
    }

    /// 在庫の外のソース (テスト用)。
    #[cfg(test)]
    pub(super) fn outside_sources(
        &self,
    ) -> impl Iterator<Item = &super::repository_audit::OutsideSource> {
        self.outside.iter().flat_map(OutsideScan::sources)
    }

    /// registry の依存からの named import を受け入れる緩和の材料。監査が成り立つときだけ使う。
    pub(super) fn relaxation(&self) -> Option<&PackageBoundary> {
        self.repository.as_ref()?;
        self.settings.as_ref().map(|settings| &settings.packages)
    }

    /// `name` を大域にリポジトリのどこかで宣言しているか。確かめられなければ true。
    pub(super) fn declares_global(&self, name: &str) -> bool {
        match (&self.repository, &self.outside) {
            (Some(repository), Some(outside)) => {
                repository.declares_global(&self.base, outside, &self.budget, name)
            }
            _ => true,
        }
    }
}

/// 対象の名前ごとの在庫: 基本の集合と、名前が現れて加えた外のファイル。
pub(super) struct InventoryView<'a> {
    base: &'a RemovalInventory,
    extra: &'a OutsideUses,
    /// 相対 import の解決先の候補 (在庫の全てのパス)。
    paths: HashMap<String, Vec<String>>,
}

impl<'a> InventoryView<'a> {
    fn new(base: &'a RemovalInventory, extra: &'a OutsideUses) -> Self {
        let paths = base
            .files
            .keys()
            .chain(extra.keys())
            .map(|path| (path.clone(), Vec::new()))
            .collect();
        Self { base, extra, paths }
    }

    fn files(&self) -> impl Iterator<Item = (&'a String, &'a RemovalFile)> {
        self.base
            .files
            .iter()
            .chain(self.extra.iter().map(|(path, file)| (path, file.as_ref())))
    }

    pub(super) fn parsed(&self) -> impl Iterator<Item = (&str, &[u8], Node<'_>, LangId)> {
        self.files().map(|(path, file)| {
            (
                path.as_str(),
                file.source.as_ref(),
                file.tree.root_node(),
                file.lang,
            )
        })
    }

    /// 在庫に読んだ `path` の内容が `source` と同じか。
    pub(super) fn has_source(&self, path: &str, source: &[u8]) -> bool {
        self.base
            .files
            .get(path)
            .or_else(|| self.extra.get(path).map(Rc::as_ref))
            .is_some_and(|file| file.source.as_ref() == source)
    }
}

/// 在庫の外と照合する語。変えた関数の名前 (`.` を含むメンバー名は除く) と、変えた TS 系の
/// ファイルを指す名前 ([`module_word`])。
pub(crate) fn optional_removal_words(
    names: &HashSet<String>,
    diff_files: &[crate::models::impact::DiffFile],
) -> Vec<String> {
    let modules = diff_files.iter().filter_map(|file| {
        let path = normalize_workspace_separators(&file.new_path);
        let ts = camino::Utf8Path::new(&path)
            .extension()
            .is_some_and(|extension| {
                ["ts", "tsx", "mts", "cts"]
                    .iter()
                    .any(|ts| extension.eq_ignore_ascii_case(ts))
            });
        ts.then(|| module_word(&path).map(str::to_owned)).flatten()
    });
    names
        .iter()
        .filter(|name| !name.contains('.'))
        .cloned()
        .chain(modules)
        .collect()
}

/// 一回の API 検出に閉じる。取得失敗も記録し、証明を再試行しない。
#[derive(Default)]
pub(crate) struct OptionalRemovalCache {
    /// 在庫の外を 1 回で読むために、検出の前に集めた語 (変えた関数の名前と、変えた TS 系の
    /// ファイルを指す名前)。ここに無い語は、必要になったときに読み直す。
    words: Vec<String>,
    evidence: Option<Option<RemovalEvidence>>,
    /// 対象の名前と宣言のファイルごとの、基本の集合の外から加えるファイル。None は証明しない。
    outside_uses: HashMap<(String, String), Option<OutsideUses>>,
}

impl OptionalRemovalCache {
    /// 1 回の検出で在庫の外と照合する語を渡す。
    pub(crate) fn with_words(words: impl IntoIterator<Item = String>) -> Self {
        let mut words: Vec<String> = words.into_iter().filter(|word| !word.is_empty()).collect();
        words.sort_unstable();
        words.dedup();
        Self {
            words,
            ..Self::default()
        }
    }

    /// 証拠 (基本の集合・在庫の外・設定・監査) を返す。基本の集合を作れなければ None。
    pub(super) fn evidence(&mut self, dir: &str) -> Option<&RemovalEvidence> {
        let Self {
            words, evidence, ..
        } = self;
        evidence
            .get_or_insert_with(|| RemovalEvidence::load(dir, words))
            .as_ref()
    }

    /// 宣言のファイル `target` にある `name` の在庫と、証拠を返す。在庫を作れなければ None。
    /// 監査が成り立たなければ、`RemovalEvidence::relaxation` が None (registry の依存の緩和を
    /// 使わない)。
    pub(super) fn inventory(
        &mut self,
        dir: &str,
        name: &str,
        target: &str,
    ) -> Option<(InventoryView<'_>, &RemovalEvidence)> {
        let Self {
            words,
            evidence,
            outside_uses,
        } = self;
        let evidence = evidence
            .get_or_insert_with(|| RemovalEvidence::load(dir, words))
            .as_ref()?;
        let extra = outside_uses
            .entry((name.to_owned(), target.to_owned()))
            .or_insert_with(|| evidence.outside_uses(name, target))
            .as_ref()?;
        Some((InventoryView::new(&evidence.base, extra), evidence))
    }
}

/// 対象の名前の出現 1 つを、利用として証明する規則。
#[derive(Clone, Copy)]
pub(super) enum UseRule<'a> {
    /// 直接呼び出しで、実引数の個数が上限以下 (末尾の省略可能引数の削除)。
    CallArity(usize),
    /// JSX 要素と、`calls_allowed` のときだけ object リテラル 1 個の直接呼び出しで、
    /// `removed` のどのキーも渡していない (省略可能 props の削除)。
    PropsWithout {
        removed: &'a HashSet<String>,
        calls_allowed: bool,
    },
}

/// 在庫の全ファイルで対象の出現をすべて規則で証明し、refs 索引の全参照が証明した出現に
/// 含まれることを確かめる。証明できた利用が 1 つも無ければ None。
pub(super) fn prove_repository_uses(
    refs: &[SymbolReference],
    inventory: &InventoryView<'_>,
    packages: Option<&PackageBoundary>,
    target: &str,
    name: &str,
    rule: UseRule<'_>,
) -> Option<()> {
    let mut proven_positions = HashSet::new();
    let mut uses = 0;
    for (path, file) in inventory.files() {
        uses += prove_file(
            file,
            path,
            target,
            name,
            rule,
            FileContext {
                paths: &inventory.paths,
                packages,
                outside: inventory.extra.contains_key(path),
            },
            &mut proven_positions,
        )?;
    }
    if uses == 0
        || refs.iter().any(|reference| {
            !proven_positions.contains(&(
                normalize_workspace_separators(&reference.path),
                reference.line,
                reference.column,
            ))
        })
    {
        return None;
    }
    Some(())
}

pub(crate) fn detect_trailing_optional_params_removed_compatible_mod(
    index: &ApiRefIndex,
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
    cache: &mut OptionalRemovalCache,
) -> Option<CompatibleApiModification> {
    if site.kind != "function" || site.name.contains('.') {
        return None;
    }
    let lang = site.lang_in(TS_ONLY_LANGS)?;
    let src = sources.get(site)?;
    let (old_tree, new_tree) = src.parse_pair(lang)?;
    let new_arity = removal_arity(
        old_tree.root_node(),
        &src.old,
        new_tree.root_node(),
        &src.new,
        site.name,
    )?;
    let refs = index.refs_for(site.name)?;
    if refs.is_empty() {
        return None;
    }
    let (inventory, evidence) = cache.inventory(site.dir, site.name, site.new_path)?;
    // 在庫は元のソースを読んでいるので、元の新版と照合する。
    if !inventory.has_source(site.new_path, src.original_new()) {
        return None;
    }
    prove_repository_uses(
        refs,
        &inventory,
        evidence.relaxation(),
        site.new_path,
        site.name,
        UseRule::CallArity(new_arity),
    )?;
    Some(site.compatible("trailing_optional_params_removed"))
}

pub(super) fn checked_nodes(root: Node<'_>) -> Option<Vec<Node<'_>>> {
    if root.has_error() {
        return None;
    }
    let mut result = Vec::new();
    let mut stack = vec![(root, 0)];
    while let Some((node, depth)) = stack.pop() {
        if depth > MAX_DEPTH || result.len() >= MAX_NODES || node.is_missing() || node.is_error() {
            return None;
        }
        result.push(node);
        if node.child_count() as usize > MAX_NODES {
            return None;
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor).map(|child| (child, depth + 1)));
    }
    Some(result)
}

/// `name` を束縛する構文の列。`collect_bindings_named` は `name` フィールドを持つノードを広く
/// 束縛として数えるので、JSX 要素 (`<Panel />` はタグ名の参照で、束縛ではない) だけを除く。
fn bindings_named<'tree>(root: Node<'tree>, source: &[u8], name: &str) -> Vec<Node<'tree>> {
    collect_bindings_named(root, source, name)
        .into_iter()
        .filter(|node| {
            !matches!(
                node.kind(),
                "jsx_opening_element" | "jsx_closing_element" | "jsx_self_closing_element"
            )
        })
        .collect()
}

/// 末尾の引数の削除は、戻り値をそのまま使う呼び出しの証拠なので、明示の戻り値型も要求する。
fn unique_function<'a>(root: Node<'a>, source: &[u8], name: &str) -> Option<(Node<'a>, Node<'a>)> {
    let (function, statement) = unique_function_declaration(root, source, name)?;
    (function.child_by_field_name("return_type")?.kind() == "type_annotation")
        .then_some((function, statement))
}

/// トップレベル (export 文の直下を含む) に名前の付いた関数宣言が 1 つだけあり、同名の別の
/// 束縛・default export・型パラメータ・`this` 引数・rest 引数が無いとき、宣言とその文を返す。
/// 戻り値型の有無は問わない (判定器ごとに要否を決める)。
pub(super) fn unique_function_declaration<'a>(
    root: Node<'a>,
    source: &[u8],
    name: &str,
) -> Option<(Node<'a>, Node<'a>)> {
    if checked_nodes(root)?
        .into_iter()
        .any(|node| escaped_identifier(node, source))
    {
        return None;
    }
    let bindings = bindings_named(root, source, name);
    let [function] = bindings.as_slice() else {
        return None;
    };
    let function = *function;
    if function.kind() != "function_declaration"
        || function.child_by_field_name("name")?.kind() != "identifier"
        || function.child_by_field_name("body")?.kind() != "statement_block"
        || function.child_by_field_name("type_parameters").is_some()
    {
        return None;
    }
    let parent = function.parent()?;
    let statement = match parent.kind() {
        "program" => function,
        "export_statement"
            if parent.parent()?.kind() == "program"
                && parent.child_by_field_name("declaration")?.id() == function.id() =>
        {
            parent
        }
        _ => return None,
    };
    let mut cursor = statement.walk();
    if statement
        .children(&mut cursor)
        .any(|child| child.kind() == "default")
    {
        return None;
    }
    let params = function.child_by_field_name("parameters")?;
    for node in checked_nodes(params)? {
        if node.kind() == "rest_pattern"
            || (matches!(node.kind(), "required_parameter" | "optional_parameter")
                && node.child_by_field_name("pattern").is_some_and(|pattern| {
                    pattern.kind() == "this" || pattern.utf8_text(source).ok() == Some("this")
                }))
        {
            return None;
        }
    }
    Some((function, statement))
}

fn removal_arity(
    old: Node<'_>,
    old_source: &[u8],
    new: Node<'_>,
    new_source: &[u8],
    name: &str,
) -> Option<usize> {
    let (old_fn, old_statement) = unique_function(old, old_source, name)?;
    let (new_fn, new_statement) = unique_function(new, new_source, name)?;
    let old_parts = ts_function_signature_parts(old_fn, old_source)?;
    let new_parts = ts_function_signature_parts(new_fn, new_source)?;
    let header = |statement: Node<'_>, function: Node<'_>, source: &[u8]| {
        signature_tokens_in_range(
            statement,
            source,
            statement.start_byte(),
            function.child_by_field_name("parameters")?.start_byte(),
        )
    };
    if header(old_statement, old_fn, old_source)? != header(new_statement, new_fn, new_source)?
        || old_parts.head != new_parts.head
        || old_parts.tail != new_parts.tail
        || old_parts.tail.0.is_empty()
        || !ts_params_prefix_same_with_optional_tail(&new_parts.params, &old_parts.params)
    {
        return None;
    }
    Some(new_parts.params.len())
}

pub(super) fn dependency_target(
    node: Node<'_>,
    source: &[u8],
    path: &str,
    paths: &HashMap<String, Vec<String>>,
) -> Option<String> {
    if !matches!(node.kind(), "string" | "template_string") {
        return None;
    }
    let specifier = static_specifier(node, source)?;
    let candidates = resolve_relative_specifier(path, specifier, paths);
    let [target] = candidates.as_slice() else {
        return None;
    };
    Some(target.clone())
}

/// 対象と別の名前だけを、通常の named import で束縛する文か。default・namespace・文字列の
/// 名前・`default as X`・対象と同じ名前 (import する名前か別名) を含む文は、解決して確かめる。
pub(super) fn imports_only_unrelated_names(statement: Node<'_>, source: &[u8], name: &str) -> bool {
    let mut cursor = statement.walk();
    let Some(clause) = statement
        .named_children(&mut cursor)
        .find(|child| child.kind() == "import_clause")
    else {
        return false;
    };
    let mut cursor = clause.walk();
    let parts: Vec<_> = clause
        .named_children(&mut cursor)
        .filter(|child| child.kind() != "comment")
        .collect();
    let [named] = parts.as_slice() else {
        return false;
    };
    if named.kind() != "named_imports" {
        return false;
    }
    let mut cursor = named.walk();
    for spec in named.named_children(&mut cursor) {
        if spec.kind() == "comment" {
            continue;
        }
        let Some(imported) = spec.child_by_field_name("name") else {
            return false;
        };
        let alias = spec.child_by_field_name("alias");
        if spec.kind() != "import_specifier"
            || imported.kind() != "identifier"
            || alias.is_some_and(|alias| alias.kind() != "identifier")
        {
            return false;
        }
        for binding in std::iter::once(imported).chain(alias) {
            if !binding
                .utf8_text(source)
                .is_ok_and(|text| !text.contains('\\') && text != name && text != "default")
            {
                return false;
            }
        }
    }
    true
}

fn direct_call_arity(identifier: Node<'_>) -> Option<usize> {
    let call = identifier.parent()?;
    if call.kind() != "call_expression"
        || call.child_by_field_name("function")?.id() != identifier.id()
        || call.child_by_field_name("type_arguments").is_some()
    {
        return None;
    }
    let mut cursor = call.walk();
    if call.children(&mut cursor).any(|child| child.kind() == "?.") {
        return None;
    }
    let arguments = call.child_by_field_name("arguments")?;
    if arguments.kind() != "arguments" {
        return None;
    }
    let mut cursor = arguments.walk();
    let mut count = 0;
    for argument in arguments.named_children(&mut cursor) {
        match argument.kind() {
            "comment" => {}
            "spread_element" => return None,
            _ => count += 1,
        }
    }
    Some(count)
}

/// 除外した JS の同名の出現が、一意なローカル関数の宣言と直接呼び出しだけか。
fn unrelated_local_function(file: &RemovalFile, name: &str, nodes: &[Node<'_>]) -> bool {
    use crate::engine::js_scope::{LocalResolution, resolve_reference_binding};

    let source = &file.source;
    let Some((function, _)) = unique_function_declaration(file.tree.root_node(), source, name)
    else {
        return false;
    };
    let Some(declaration) = function.child_by_field_name("name") else {
        return false;
    };
    nodes.iter().all(|node| {
        if !matches!(
            node.kind(),
            "identifier"
                | "type_identifier"
                | "property_identifier"
                | "shorthand_property_identifier"
                | "shorthand_property_identifier_pattern"
                | "statement_identifier"
        ) || node.utf8_text(source).ok() != Some(name)
            || node.id() == declaration.id()
        {
            return true;
        }
        let position = node.start_position();
        direct_call_arity(*node).is_some()
            && resolve_reference_binding(
                &file.tree,
                source,
                file.lang,
                name,
                position.row,
                position.column,
            ) == LocalResolution::SameFileFunction
    })
}

/// 在庫のファイルを証明するときに共有する、依存の解決の材料。
#[derive(Clone, Copy)]
struct FileContext<'a> {
    paths: &'a HashMap<String, Vec<String>>,
    /// registry の依存を見分ける材料。None なら解決できない import を 1 つも許さない。
    packages: Option<&'a PackageBoundary>,
    outside: bool,
}

fn prove_file(
    file: &RemovalFile,
    path: &str,
    target: &str,
    name: &str,
    rule: UseRule<'_>,
    context: FileContext<'_>,
    positions: &mut HashSet<(String, usize, usize)>,
) -> Option<usize> {
    let FileContext {
        paths,
        packages,
        outside,
    } = context;
    let root = file.tree.root_node();
    let source = &file.source;
    let nodes = checked_nodes(root)?;
    let props_rule = matches!(rule, UseRule::PropsWithout { .. });
    // JSX の名前空間を作る・公開するファイルは、別名の公開や runtime の差し替えを通して、
    // 属性を書かない要素の型検査も変え得る。
    if props_rule && declares_jsx_name(root, source) {
        return None;
    }
    let mut import_names = HashSet::new();
    let mut import_statements = HashSet::new();
    for node in &nodes {
        if escaped_identifier(*node, source) || dynamic_execution_entry(*node, source) {
            return None;
        }
        // JSX の名前空間や factory を差し替えると、属性を書かない要素の型検査も変わり得る。
        if props_rule && replaces_jsx_environment(*node, source) {
            return None;
        }
        // 動的なモジュール列挙と require の別名化は依存を閉じられない。
        if node.kind() == "meta_property" {
            let mut cursor = node.walk();
            if node
                .children(&mut cursor)
                .any(|child| child.kind() == "import")
            {
                return None;
            }
        }
        if node.kind() == "identifier" && node.utf8_text(source).ok()? == "require" {
            let parent = node.parent()?;
            if parent.kind() != "call_expression"
                || parent.child_by_field_name("function")?.id() != node.id()
            {
                return None;
            }
        }
        match node.kind() {
            "import_statement" | "export_statement" => {
                let resolved = match node.child_by_field_name("source") {
                    None => None,
                    Some(specifier) => match dependency_target(specifier, source, path, paths) {
                        Some(resolved) => Some(resolved),
                        // registry の依存から、対象と別の名前だけを束縛する named import は、
                        // 対象を運ばない (Issue #175 の輸送の監査と同じ構文上の規則)。ワークスペース
                        // や `file:` の依存は、node_modules の中から対象を別名で再 export し得る。
                        None if node.kind() == "import_statement"
                            && imports_only_unrelated_names(*node, source, name)
                            && static_specifier(specifier, source).is_some_and(|specifier| {
                                packages.is_some_and(|packages| {
                                    packages.is_registry_dependency(specifier)
                                })
                            }) =>
                        {
                            None
                        }
                        None => return None,
                    },
                };
                if resolved.as_deref() == Some(target) {
                    if node.kind() != "import_statement" {
                        return None;
                    }
                    let mut cursor = node.walk();
                    if node
                        .children(&mut cursor)
                        .any(|child| child.kind() == "type")
                    {
                        return None;
                    }
                    let clause = node
                        .named_children(&mut cursor)
                        .find(|child| child.kind() == "import_clause")?;
                    let mut cursor = clause.walk();
                    let children: Vec<_> = clause
                        .named_children(&mut cursor)
                        .filter(|child| child.kind() != "comment")
                        .collect();
                    let [named] = children.as_slice() else {
                        return None;
                    };
                    if named.kind() != "named_imports" {
                        return None;
                    }
                    let mut cursor = named.walk();
                    for spec in named.named_children(&mut cursor) {
                        if spec.kind() == "comment" {
                            continue;
                        }
                        if spec.kind() != "import_specifier"
                            || spec.child_by_field_name("alias").is_some()
                        {
                            return None;
                        }
                        let mut cursor = spec.walk();
                        if spec
                            .children(&mut cursor)
                            .any(|child| child.kind() == "type")
                        {
                            return None;
                        }
                        let imported = spec.child_by_field_name("name")?;
                        if imported.kind() != "identifier" {
                            return None;
                        }
                        if imported.utf8_text(source).ok()? == name {
                            import_names.insert(imported.id());
                            import_statements.insert(node.id());
                        }
                    }
                }
            }
            "import_alias" | "import_require_clause" => return None,
            "internal_module" | "module" => {
                // `*` を含む名前 (`"*.svg"` など) はパターンの ambient module で、ファイルに解決
                // できなかった指定子にだけ当たる。ファイルに解決される対象のモジュールは変えない。
                if let Some(specifier) = node.child_by_field_name("name")
                    && specifier.kind() == "string"
                    && !static_specifier(specifier, source).is_some_and(|name| name.contains('*'))
                    && dependency_target(specifier, source, path, paths)? == target
                {
                    return None;
                }
            }
            "call_expression" => {
                let function = node.child_by_field_name("function")?;
                if function.kind() == "import"
                    || (function.kind() == "identifier"
                        && function.utf8_text(source).ok()? == "require")
                {
                    let arguments = node.child_by_field_name("arguments")?;
                    let mut cursor = arguments.walk();
                    let first = arguments
                        .named_children(&mut cursor)
                        .find(|child| child.kind() != "comment")?;
                    if dependency_target(first, source, path, paths)? == target {
                        return None;
                    }
                }
            }
            _ => {}
        }
    }
    if outside
        && file.lang == LangId::Javascript
        && import_names.is_empty()
        && unrelated_local_function(file, name, &nodes)
    {
        return Some(0);
    }
    let bindings = bindings_named(root, source, name);
    let declaration_name = if path == target {
        // 宣言の契約 (戻り値型の要否を含む) は判定器が旧新のソースで確かめ済み。
        unique_function_declaration(root, source, name)?
            .0
            .child_by_field_name("name")
            .map(|node| node.id())
    } else {
        if !bindings.is_empty()
            && (import_names.len() != 1
                || bindings.iter().any(|binding| {
                    !import_statements.contains(&binding.id())
                        && !(binding.kind() == "import_specifier"
                            && binding
                                .child_by_field_name("name")
                                .is_some_and(|node| import_names.contains(&node.id())))
                }))
        {
            return None;
        }
        None
    };
    let mut uses = 0;
    for node in nodes {
        if !matches!(
            node.kind(),
            "identifier"
                | "type_identifier"
                | "property_identifier"
                | "shorthand_property_identifier"
                | "shorthand_property_identifier_pattern"
                | "statement_identifier"
        ) || node.utf8_text(source).ok()? != name
        {
            continue;
        }
        if !matches!(file.lang, LangId::Typescript | LangId::Tsx) {
            return None;
        }
        let position = node.start_position();
        positions.insert((path.to_string(), position.row, position.column));
        if declaration_name == Some(node.id()) || import_names.contains(&node.id()) {
            continue;
        }
        if path != target && import_names.is_empty() {
            return None;
        }
        if node.kind() != "identifier" {
            return None;
        }
        match rule {
            UseRule::CallArity(arity) => {
                if direct_call_arity(node)? > arity {
                    return None;
                }
            }
            UseRule::PropsWithout {
                removed,
                calls_allowed,
            } => {
                if !props_use_omits(node, source, removed, calls_allowed) {
                    return None;
                }
            }
        }
        uses += 1;
    }
    Some(uses)
}

/// 対象の名前の識別子が、`removed` のどのキーも渡さない利用か。JSX 要素はタグ名の位置で
/// 属性と children を読み、直接呼び出しは `calls_allowed` のときだけ object リテラルを読む。
fn props_use_omits(
    identifier: Node<'_>,
    source: &[u8],
    removed: &HashSet<String>,
    calls_allowed: bool,
) -> bool {
    if crate::engine::jsx::jsx_name_opening_tag(identifier, source).is_some() {
        // 構文エラーのあるファイルは在庫に入らないので、ここでは解析済みとして読む。
        return crate::engine::jsx::caller_shape(identifier, source, true)
            .is_some_and(|shape| shape.omits(removed));
    }
    calls_allowed && object_argument_omits(identifier, source, removed)
}

/// 型引数・`?.`・spread の無い直接呼び出しで、引数がちょうど 1 個の object リテラルであり、
/// その要素が通常のキーの組と shorthand だけで、どれも `removed` に含まれないか。
fn object_argument_omits(identifier: Node<'_>, source: &[u8], removed: &HashSet<String>) -> bool {
    let Some(call) = identifier.parent() else {
        return false;
    };
    if call.kind() != "call_expression"
        || call
            .child_by_field_name("function")
            .is_none_or(|function| function.id() != identifier.id())
        || call.child_by_field_name("type_arguments").is_some()
    {
        return false;
    }
    let mut cursor = call.walk();
    if call.children(&mut cursor).any(|child| child.kind() == "?.") {
        return false;
    }
    let Some(arguments) = call
        .child_by_field_name("arguments")
        .filter(|arguments| arguments.kind() == "arguments")
    else {
        return false;
    };
    let mut cursor = arguments.walk();
    let values: Vec<_> = arguments
        .named_children(&mut cursor)
        .filter(|child| child.kind() != "comment")
        .collect();
    let [object] = values.as_slice() else {
        return false;
    };
    object_omits(*object, source, removed)
}

pub(super) fn object_omits(object: Node<'_>, source: &[u8], removed: &HashSet<String>) -> bool {
    if object.kind() != "object" {
        return false;
    }
    let mut cursor = object.walk();
    object.named_children(&mut cursor).all(|entry| {
        let key = match entry.kind() {
            "comment" => return true,
            "pair" => match entry.child_by_field_name("key") {
                Some(key) if key.kind() == "property_identifier" => key,
                _ => return false,
            },
            "shorthand_property_identifier" => entry,
            _ => return false,
        };
        key.utf8_text(source)
            .is_ok_and(|text| !text.contains('\\') && !removed.contains(text))
    })
}

pub(super) fn dynamic_execution_entry(node: Node<'_>, source: &[u8]) -> bool {
    if matches!(node.kind(), "identifier" | "property_identifier")
        && matches!(node.utf8_text(source).ok(), Some("eval" | "Function"))
    {
        return true;
    }
    // グローバルの添字アクセスは、既知の実行入口の別名になり得る。
    node.kind() == "subscript_expression"
        && node.child_by_field_name("object").is_some_and(|object| {
            object.kind() == "identifier"
                && matches!(
                    object.utf8_text(source).ok(),
                    Some("globalThis" | "window" | "self" | "global")
                )
        })
}

pub(super) fn escaped_identifier(node: Node<'_>, source: &[u8]) -> bool {
    matches!(
        node.kind(),
        "identifier"
            | "type_identifier"
            | "property_identifier"
            | "private_property_identifier"
            | "shorthand_property_identifier"
            | "shorthand_property_identifier_pattern"
            | "statement_identifier"
    ) && node.utf8_text(source).is_ok_and(|text| text.contains('\\'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_removal_requires_unique_explicit_declaration_contract() {
        for lang in [LangId::Typescript, LangId::Tsx] {
            let old = "export function spacing(size: number, scale = 1): number { return size; }";
            let old_tree = parser::parse_source(old.as_bytes(), lang).unwrap();
            for (new, accepted) in [
                (
                    "export function spacing(size: number): number { return size; }",
                    true,
                ),
                (
                    "export function spacing(size: string): number { return 1; }",
                    false,
                ),
                (
                    "export function spacing(size: number): string { return ''; }",
                    false,
                ),
                (
                    "export function spacing(size: number) { return size; }",
                    false,
                ),
                (
                    "export default function spacing(size: number): number { return size; }",
                    false,
                ),
                (
                    "export function spacing(size: number): number { return size; } type spacing = number;",
                    false,
                ),
                (
                    "export function spacing(size: number): number; export function spacing(size: number): number { return size; }",
                    false,
                ),
                (
                    "export function spacing(size: number): number { return size; } const broken = ;",
                    false,
                ),
            ] {
                let tree = parser::parse_source(new.as_bytes(), lang).unwrap();
                assert_eq!(
                    removal_arity(
                        old_tree.root_node(),
                        old.as_bytes(),
                        tree.root_node(),
                        new.as_bytes(),
                        "spacing"
                    )
                    .is_some(),
                    accepted,
                    "{lang:?}: {new}"
                );
            }
        }
    }

    #[test]
    fn optional_removal_named_import_relaxation_is_syntactic() {
        for (source, relaxed) in [
            ("import { useState } from 'react';", true),
            ("import { useState as useLocal } from 'react';", true),
            ("import type { FC } from 'react';", true),
            ("import { type FC, useMemo } from 'react';", true),
            ("import { /* none */ } from 'react';", true),
            ("import { Panel } from 'react';", false),
            ("import { Panel as Local } from 'react';", false),
            ("import { Local as Panel } from 'react';", false),
            ("import { 'a-b' as local } from 'react';", false),
            ("import { default as Local } from 'react';", false),
            ("import React, { useState } from 'react';", false),
            ("import React from 'react';", false),
            ("import * as React from 'react';", false),
            ("import 'react';", false),
        ] {
            let tree = parser::parse_source(source.as_bytes(), LangId::Tsx).unwrap();
            let statement = tree.root_node().named_child(0).unwrap();
            assert_eq!(statement.kind(), "import_statement", "{source}");
            assert_eq!(
                imports_only_unrelated_names(statement, source.as_bytes(), "Panel"),
                relaxed,
                "{source}"
            );
        }
    }

    #[test]
    fn optional_removal_counts_only_exact_direct_call_arguments() {
        for lang in [LangId::Typescript, LangId::Tsx] {
            for (source, expected) in [
                ("spacing();", Some(0)),
                ("spacing(/* before */ 1, /* tail */);", Some(1)),
                ("spacing(undefined);", Some(1)),
                ("spacing(1, undefined);", Some(2)),
                ("spacing(...values);", None),
                ("(spacing)();", None),
                ("new spacing();", None),
                ("consume(spacing);", None),
                ("spacing?.();", None),
                ("spacing<string>();", None),
                ("spacing.call(null);", None),
            ] {
                let tree = parser::parse_source(source.as_bytes(), lang).unwrap();
                let identifier = checked_nodes(tree.root_node())
                    .unwrap()
                    .into_iter()
                    .find(|node| {
                        node.kind() == "identifier"
                            && node.utf8_text(source.as_bytes()).unwrap() == "spacing"
                    })
                    .unwrap();
                assert_eq!(
                    direct_call_arity(identifier),
                    expected,
                    "{lang:?}: {source}"
                );
            }
        }
    }

    /// `Panel` の最初の識別子の出現が、`legacy` を渡さない利用として証明できるか。
    fn props_use(source: &str, calls_allowed: bool, removed: &[&str]) -> bool {
        let tree = parser::parse_source(source.as_bytes(), LangId::Tsx).unwrap();
        assert!(!tree.root_node().has_error(), "{source}");
        let identifier = checked_nodes(tree.root_node())
            .unwrap()
            .into_iter()
            .filter(|node| {
                node.kind() == "identifier" && node.utf8_text(source.as_bytes()).unwrap() == "Panel"
            })
            .min_by_key(|node| node.start_byte())
            .unwrap();
        let removed: HashSet<String> = removed.iter().map(|key| key.to_string()).collect();
        props_use_omits(identifier, source.as_bytes(), &removed, calls_allowed)
    }

    #[test]
    fn optional_props_removal_classifies_jsx_and_call_sites() {
        for (source, calls_allowed, expected) in [
            ("const v = <Panel label=\"x\" />;", false, true),
            ("const v = <Panel label=\"x\"></Panel>;", false, true),
            ("const v = <Panel>text</Panel>;", false, true),
            ("const v = <Panel legacy={{ id: 'a' }} />;", false, false),
            ("const v = <Panel legacy />;", false, false),
            (
                "const v = <Panel legacy={undefined}></Panel>;",
                false,
                false,
            ),
            ("const v = <Panel {...props} />;", false, false),
            ("const v = <Panel ns:legacy=\"x\" />;", false, false),
            ("const v = <Panel<string> />;", false, false),
            ("const v = <Panel label=\"a\" label=\"b\" />;", false, false),
            ("Panel({ label: 'x' });", true, true),
            ("Panel({ label, /* note */ onDone });", true, true),
            ("Panel({ label: 'x' });", false, false),
            ("Panel({ legacy: 1 });", true, false),
            ("Panel({ legacy });", true, false),
            ("Panel({ 'legacy': 1 });", true, false),
            ("Panel({ [key]: 1 });", true, false),
            ("Panel({ ...rest });", true, false),
            ("Panel({ legacy() { return 1; } });", true, false),
            ("Panel(props);", true, false),
            ("Panel();", true, false),
            ("Panel({}, 1);", true, false),
            ("Panel?.({});", true, false),
            ("Panel<string>({});", true, false),
            ("new Panel({});", true, false),
            ("const C = Panel;", true, false),
            ("type P = typeof Panel;", true, false),
        ] {
            assert_eq!(
                props_use(source, calls_allowed, &["legacy"]),
                expected,
                "{source}"
            );
        }
        // children を消すときは、終了タグまでの中身 (空白を含む) も渡す。
        assert!(props_use(
            "const v = <Panel></Panel>;",
            false,
            &["children"]
        ));
        for source in [
            "const v = <Panel> </Panel>;",
            "const v = <Panel>text</Panel>;",
            "const v = <Panel children=\"x\" />;",
        ] {
            assert!(!props_use(source, false, &["children"]), "{source}");
        }
    }

    #[test]
    fn optional_props_removal_detects_jsx_name_declarations() {
        for (source, declared) in [
            (
                "namespace RuntimeTypes { export interface Element {} }\nexport { RuntimeTypes as JSX };",
                true,
            ),
            ("export { JSX } from './runtime';", true),
            ("export * as JSX from './runtime';", true),
            ("import JSX = Runtime.Types;", true),
            ("import { Types as JSX } from './runtime';", true),
            ("import * as JSX from './runtime';", true),
            ("type JSX = { Element: unknown };", true),
            ("interface JSX {}", true),
            ("namespace JSX {}", true),
            ("const JSX = {};", true),
            (
                "export function view(): JSX.Element { return null as never; }",
                false,
            ),
            ("const v: JSX.IntrinsicElements['div'] = {};", false),
            ("export { Runtime as Other };", false),
        ] {
            let tree = parser::parse_source(source.as_bytes(), LangId::Tsx).unwrap();
            assert!(!tree.root_node().has_error(), "{source}");
            assert_eq!(
                declares_jsx_name(tree.root_node(), source.as_bytes()),
                declared,
                "{source}"
            );
        }
    }

    #[test]
    fn optional_props_removal_detects_replaced_jsx_environment() {
        for (source, replaced) in [
            (
                "declare global { namespace JSX { interface Element {} } } export {};",
                true,
            ),
            ("declare namespace JSX { interface Element {} }", true),
            ("namespace Outer.JSX {}", true),
            ("/** @jsxImportSource preact */ const v = 1;", true),
            ("/* @jsx h */ const v = 1;", true),
            ("// @jsxFrag Fragment\nconst v = 1;", true),
            (
                "declare global { interface Window { app: unknown } } export {};",
                false,
            ),
            ("namespace App {}", false),
            ("// jsx in plain words\nconst v = 1;", false),
        ] {
            let tree = parser::parse_source(source.as_bytes(), LangId::Tsx).unwrap();
            let found = checked_nodes(tree.root_node())
                .unwrap()
                .into_iter()
                .any(|node| replaces_jsx_environment(node, source.as_bytes()));
            assert_eq!(found, replaced, "{source}");
        }
    }

    #[test]
    fn optional_removal_module_and_call_node_contracts_exist() {
        let fixture = include_str!("../../../../tests/fixtures/optional_removal_contract.tsx");
        let tree = parser::parse_source(fixture.as_bytes(), LangId::Tsx).unwrap();
        assert!(!tree.root_node().has_error());
        for lang in [LangId::Typescript, LangId::Tsx] {
            let grammar = lang.ts_language();
            for query in [
                "(function_declaration name: (identifier) parameters: (formal_parameters) return_type: (type_annotation) body: (statement_block))",
                "(required_parameter)",
                "(optional_parameter)",
                "(rest_pattern)",
                "(import_statement source: (string))",
                "(import_specifier name: (identifier))",
                "(import_specifier alias: (identifier))",
                "(namespace_import)",
                "(import_require_clause)",
                "(import_alias)",
                "(internal_module)",
                "(module name: (string))",
                "(meta_property \"import\")",
                "(call_expression function: (identifier) arguments: (arguments))",
                "(call_expression type_arguments: (type_arguments))",
                "(call_expression \"?.\")",
                "(spread_element)",
                "(new_expression)",
                "(identifier)",
                "(type_identifier)",
                "(property_identifier)",
                "(private_property_identifier)",
                "(shorthand_property_identifier)",
                "(shorthand_property_identifier_pattern)",
                "(statement_identifier)",
                "(subscript_expression object: (identifier) index: (string))",
                "(export_statement source: (string))",
            ] {
                tree_sitter::Query::new(&grammar, query)
                    .unwrap_or_else(|error| panic!("{lang:?}: {query}: {error}"));
            }
        }
    }
}
