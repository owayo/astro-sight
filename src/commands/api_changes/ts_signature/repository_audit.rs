//! props 削除と省略可能引数削除の証明が使う、信頼の境界 (Issue #208)。
//!
//! 利用の在庫は、ignore と隠しファイルの除外に従う集合 (基本の集合) を全部解析する。基本の集合から
//! 外れたファイル (node_modules と .git の中を除く) は、TS / JS 系と埋め込みのソースだけを中身で
//! 絞り込み、対象の名前が現れるものを在庫に加える。リポジトリの監査は、ignore に関係なく全ての
//! ファイルを対象にし、基本の集合の外は中身の印で絞り込んだものだけを解析する。監査の対象は
//! ソース・`.d.ts`・tsconfig / jsconfig・package.json である。registry から入れた依存のパッケージ
//! (node_modules の中身) は、リポジトリのソースに依存せず、React の標準の JSX の契約を変えない
//! ものとして信頼する。どちらとも決められないもの (読めない設定、ワークスペースや `file:` の依存、
//! パスの別名、外や node_modules のファイルを取り込む symlink・設定・reference など) は証明しない。

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use tree_sitter::Node;

use super::outside::{
    OutsideScan, audit_sources, checks_specifiers, parse_outside, reaches_outside,
};
use super::removed_params::{RemovalInventory, escaped_identifier};
use super::source_scan::{Budget, Needle, Needles, ScanLimits, scan_file};
use crate::commands::api_changes::ref_index::contains_js_word;
use crate::commands::api_changes::ts_const_arg::collect_bindings_named;
pub(super) use crate::commands::api_changes::ts_module_paths::normalize_lexically;
use crate::commands::api_changes::ts_module_paths::{
    RESOLVED_EXTENSIONS, file_candidates, package_name, registry_spec,
};
use crate::commands::dead_code_default_liveness::static_specifier;
use crate::engine::bounded_read::read_utf8_file_limited;
use crate::engine::tree_walk::{Children, Walk, any_node, walk_preorder};
use crate::git_support::normalize_workspace_separators;
use crate::language::LangId;

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_CONFIG_TOTAL_BYTES: usize = 8 * 1024 * 1024;
const MAX_CHAIN_DEPTH: usize = 8;
const MAX_ANCESTORS: usize = 128;
const PROJECT_CONFIG_NAMES: [&str; 2] = ["tsconfig.json", "jsconfig.json"];
const MODULE_FILE_EXTENSIONS: [&str; 13] = [
    "", ".ts", ".tsx", ".d.ts", ".mts", ".cts", ".d.mts", ".d.cts", ".js", ".jsx", ".mjs", ".cjs",
    ".json",
];
/// 自動の JSX runtime の import 元 (どれも `react` のパッケージに解決される)。
const REACT_RUNTIME: [&str; 3] = ["react", "react/jsx-runtime", "react/jsx-dev-runtime"];
/// TypeScript が reference の `path` に補う拡張子。
const REFERENCE_EXTENSIONS: [&str; 3] = [".ts", ".tsx", ".d.ts"];
/// JS / TS を埋め込むソースの拡張子。
const EMBEDDED_EXTENSIONS: [&str; 4] = ["vue", "svelte", "astro", "mdx"];
/// React の環境の全体の監査 ([`ReactEnvironment`]) の上限。依存の指定子を確かめるため、TS の
/// ソースはほぼ全部を解析する (構文木は 1 ファイルずつ捨てる)。
const REACT_ENVIRONMENT_LIMITS: ScanLimits = ScanLimits {
    stream_bytes: ScanLimits::DEFAULT.stream_bytes,
    parse_bytes: 512 * 1024 * 1024,
};
/// 入る依存の版と解決先を決める、設定と package.json のほかのファイル (ロックファイル、
/// Yarn の PnP、lerna のワークスペースの定義)。
const DEPENDENCY_STATE_NAMES: [&str; 10] = [
    "package-lock.json",
    "npm-shrinkwrap.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lock",
    "bun.lockb",
    ".yarnrc",
    ".pnp.cjs",
    ".pnp.loader.mjs",
    "lerna.json",
];

/// 解析するディレクトリを、node_modules と .git の中を除き、ignore の指定に関係なく走査した結果。
pub(super) struct RepositoryFiles {
    /// `dir` を絶対パスにしたもの (symlink は解決しない)。
    logical: PathBuf,
    /// 正規化した `dir`。
    root: PathBuf,
    configs: Vec<PathBuf>,
    manifests: Vec<PathBuf>,
    /// pnpm-workspace.yaml と .yarnrc.yml (依存の指定を差し替え得る設定)。
    package_configs: Vec<PathBuf>,
    /// 依存の指定を任意に書き換えられる pnpmfile があった (読めない・symlink のパッケージ
    /// マネージャの設定を含む)。
    pnpmfile: bool,
    /// .npmrc。
    npmrcs: Vec<PathBuf>,
    /// 在庫の基本の集合から外れた、TS / JS 系と埋め込みのソース。
    pub(super) outside: Vec<OutsideSource>,
    /// 先を監査できない symlink (ディレクトリを指すもの、設定・package.json・ソースになり得るもの)
    /// があった。
    symlinked: bool,
}

/// 在庫の基本の集合 (ignore と隠しファイルの除外に従う集合) から外れたソース。
pub(super) struct OutsideSource {
    pub(super) path: PathBuf,
    /// `dir` 相対のパス (`/` 区切り)。
    pub(super) relative: String,
    /// TS / JS 系の言語。埋め込みのソース (`.vue` など) は None。
    pub(super) lang: Option<LangId>,
}

enum FileRole {
    Config,
    Manifest,
    /// pnpm-workspace.yaml・.yarnrc.yml。
    PackageConfig,
    /// 依存の指定を任意に書き換えられる pnpm の hook。
    Pnpmfile,
    /// .npmrc (pnpm の hook の場所を指定し得る)。
    Npmrc,
}

/// pnpm が読む hook のファイルの名前。
const PNPMFILE_NAMES: [&str; 4] = [
    ".pnpmfile.cjs",
    ".pnpmfile.mjs",
    ".pnpmfile.js",
    "pnpmfile.js",
];
/// 依存の指定を差し替え得る、パッケージマネージャの設定の名前。
const PACKAGE_CONFIG_NAMES: [&str; 2] = ["pnpm-workspace.yaml", ".yarnrc.yml"];

fn file_role(name: &str) -> Option<FileRole> {
    if (name.starts_with("tsconfig") || name.starts_with("jsconfig")) && name.ends_with(".json") {
        return Some(FileRole::Config);
    }
    if PACKAGE_CONFIG_NAMES.contains(&name) {
        return Some(FileRole::PackageConfig);
    }
    if PNPMFILE_NAMES.contains(&name) {
        return Some(FileRole::Pnpmfile);
    }
    if name == ".npmrc" {
        return Some(FileRole::Npmrc);
    }
    (name == "package.json").then_some(FileRole::Manifest)
}

/// 拡張子が TS / JS 系なら `Some(Some(言語))`、埋め込みのソースなら `Some(None)`。ほかの言語の
/// ソースは TS の型検査に関わらないので None。
pub(super) fn source_language(name: &str) -> Option<Option<LangId>> {
    let extension = Path::new(name).extension()?.to_str()?;
    if EMBEDDED_EXTENSIONS
        .iter()
        .any(|embedded| extension.eq_ignore_ascii_case(embedded))
    {
        return Some(None);
    }
    match LangId::from_path(camino::Utf8Path::new(name)) {
        Ok(lang @ (LangId::Typescript | LangId::Tsx | LangId::Javascript)) => Some(Some(lang)),
        _ => None,
    }
}

/// 名前から、在庫か監査の対象になり得るか (設定・package.json・TS / JS 系と埋め込みのソース)。
fn relevant_name(name: &str) -> bool {
    file_role(name).is_some() || source_language(name).is_some()
}

/// symlink の先は走査しないので、監査できない。ディレクトリを指すものと、名前から在庫・監査の
/// 対象になり得るもの (壊れたものを含む) なら true。
fn symlink_hides_files(path: &Path, name: &str) -> bool {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => true,
        Ok(_) => relevant_name(name),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => relevant_name(name),
        Err(_) => true,
    }
}

impl RepositoryFiles {
    /// `base` (在庫の基本の集合の絶対パス) に無い TS / JS 系と埋め込みのソースを `outside` に
    /// 集める。ファイル数に上限は掛けない。権限が無くて開けないディレクトリは、同じ利用者が動かす
    /// 型検査も読めないので外す。ほかの読めないエントリと、対象になり得る名前の UTF-8 でない
    /// ファイルや FIFO などがあれば None。
    pub(super) fn walk(dir: &str, base: &HashSet<PathBuf>) -> Option<Self> {
        let logical = std::path::absolute(Path::new(dir)).ok()?;
        let root = logical.canonicalize().ok()?;
        let mut builder = ignore::WalkBuilder::new(&root);
        builder
            .standard_filters(false)
            .follow_links(false)
            .filter_entry(|entry| {
                entry.depth() == 0
                    || !matches!(entry.file_name().to_str(), Some("node_modules" | ".git"))
            });
        let mut files = Self {
            logical,
            root: root.clone(),
            configs: Vec::new(),
            manifests: Vec::new(),
            package_configs: Vec::new(),
            pnpmfile: false,
            npmrcs: Vec::new(),
            outside: Vec::new(),
            symlinked: false,
        };
        for entry in builder.build() {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error)
                    if error.io_error().is_some_and(|error| {
                        error.kind() == std::io::ErrorKind::PermissionDenied
                    }) =>
                {
                    continue;
                }
                Err(_) => return None,
            };
            if entry.depth() == 0 {
                continue;
            }
            let Some(name) = entry.file_name().to_str() else {
                if relevant_name(&entry.file_name().to_string_lossy()) {
                    return None;
                }
                continue;
            };
            let kind = entry.file_type()?;
            // 役割の判定より先に symlink を調べる。ディレクトリの symlink の中は走査しないので、
            // 黙って外さずに監査を作らない理由にする。
            if entry.path_is_symlink() || kind.is_symlink() {
                files.symlinked |= symlink_hides_files(entry.path(), name);
                // 中身を読めないパッケージマネージャの設定は、依存の指定を書き換え得る。
                files.pnpmfile |= matches!(
                    file_role(name),
                    Some(FileRole::PackageConfig | FileRole::Pnpmfile | FileRole::Npmrc)
                );
                continue;
            }
            if kind.is_dir() {
                continue;
            }
            if !kind.is_file() {
                // FIFO などは読まない。対象になり得る名前なら、在庫も監査も閉じられない。
                if relevant_name(name) {
                    return None;
                }
                continue;
            }
            let path = entry.path();
            match file_role(name) {
                Some(FileRole::Config) => files.configs.push(path.to_path_buf()),
                Some(FileRole::Manifest) => files.manifests.push(path.to_path_buf()),
                Some(FileRole::PackageConfig) => files.package_configs.push(path.to_path_buf()),
                Some(FileRole::Pnpmfile) => files.pnpmfile = true,
                Some(FileRole::Npmrc) => files.npmrcs.push(path.to_path_buf()),
                None => {}
            }
            if let Some(lang) = source_language(name)
                && !base.contains(path)
            {
                let relative =
                    normalize_workspace_separators(path.strip_prefix(&root).ok()?.to_str()?);
                files.outside.push(OutsideSource {
                    path: path.to_path_buf(),
                    relative,
                    lang,
                });
            }
        }
        Some(files)
    }
}

/// 設定 (tsconfig / jsconfig) と package.json から読んだ、監査の前提と依存の判定の材料。
pub(super) struct Settings {
    /// 設定と依存が、JSX の型環境と runtime の解決先を差し替えていないか。
    jsx_standard: bool,
    pub(super) packages: PackageBoundary,
    /// 読んだ tsconfig / jsconfig (`extends`・`references` の先と祖先の設定を含む。正規化した
    /// パス)。
    config_files: BTreeSet<PathBuf>,
    /// どれかの設定が `allowJs`・`checkJs` を有効にした (JS 系のファイルも TypeScript の
    /// プログラムに入る)。
    pub(super) allow_js: bool,
}

impl Settings {
    /// 読めない・解釈できない設定や package.json、外や node_modules を取り込む設定、外の
    /// ワークスペースの定義があれば None。
    pub(super) fn load(files: &RepositoryFiles) -> Option<Self> {
        // pnpmfile は依存の指定を任意に書き換えられるので、どの依存も registry と決められない。
        // .npmrc の `pnpmfile`・`global-pnpmfile` は、別の名前の hook を指定する。
        if files.pnpmfile {
            return None;
        }
        let mut npmrc_bytes = 0;
        for path in &files.npmrcs {
            npmrc_without_hooks(path, &mut npmrc_bytes)?;
        }
        for base in [&files.logical, &files.root] {
            for (depth, ancestor) in base.ancestors().enumerate().skip(1) {
                if depth > MAX_ANCESTORS {
                    return None;
                }
                let npmrc = ancestor.join(".npmrc");
                if exists(&npmrc)? {
                    npmrc_without_hooks(&npmrc, &mut npmrc_bytes)?;
                }
            }
        }
        let mut configs = ConfigScan::new(files.root.clone());
        for path in &files.configs {
            configs.scan(path, 0)?;
        }
        // TypeScript はファイルからいちばん近い tsconfig.json / jsconfig.json を使う。直下に
        // 無ければ、祖先の設定が配下のファイルに効き得る。
        if !PROJECT_CONFIG_NAMES
            .iter()
            .any(|name| files.configs.contains(&files.root.join(name)))
        {
            for ancestor_base in [&files.logical, &files.root] {
                for (depth, ancestor) in ancestor_base.ancestors().enumerate().skip(1) {
                    if depth > MAX_ANCESTORS {
                        return None;
                    }
                    if configs.scan_project_directory(ancestor)? {
                        break;
                    }
                }
            }
        }
        configs.expand_deferred_bases()?;
        let mappings_resolved = configs.resolve_mapping_uses();
        let packages = PackageBoundary::load(files, &configs)?;
        let jsx_standard = configs.standard
            && mappings_resolved
            && configs.mappings_stay_inside()
            && configs.deferred_stay_inside()
            && packages.entries_stay_inside()
            && packages.standard_react()
            && configs
                .types
                .iter()
                .all(|name| packages.registry_types(name));
        Some(Self {
            jsx_standard,
            packages,
            config_files: configs.checked,
            allow_js: configs.allow_js,
        })
    }
}

/// `.npmrc` が、依存の指定を書き換える hook の場所 (`pnpmfile`・`global-pnpmfile`) を指定して
/// いないか。読めないとき・指定しているときは None。中身は鍵の名前だけを見る (値は読まない)。
fn npmrc_without_hooks(path: &Path, bytes: &mut usize) -> Option<()> {
    regular_file(path)?;
    let source = read_utf8_file_limited(path, MAX_CONFIG_BYTES)?;
    *bytes = bytes.checked_add(source.len())?;
    if *bytes > MAX_CONFIG_TOTAL_BYTES {
        return None;
    }
    for line in source.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        let key = line
            .split_once('=')
            .map_or(line, |(key, _)| key)
            .trim()
            .trim_matches(['"', '\'']);
        if ["pnpmfile", "global-pnpmfile", "globalPnpmfile"]
            .iter()
            .any(|hook| key.eq_ignore_ascii_case(hook))
        {
            return None;
        }
    }
    Some(())
}

/// 基本の集合とその外のファイルを、ignore の指定に関係なく調べた結果。
pub(super) struct RepositoryAudit {
    /// 設定・依存・ソースが、JSX の型環境と runtime の解決先を差し替えていないか。
    jsx_standard: bool,
    globals: RefCell<BTreeMap<String, bool>>,
}

impl RepositoryAudit {
    /// 先を監査できない symlink があるとき、設定や package.json を読めないとき (`settings` が
    /// None)、基本の集合の外を監査できなかったとき (`outside_standard` が None)、基本の集合の
    /// ソースが外や node_modules のファイルを取り込むときは None (どの証明にも使わない)。
    pub(super) fn load(
        files: &RepositoryFiles,
        settings: Option<&Settings>,
        base: &RemovalInventory,
        outside_standard: Option<bool>,
    ) -> Option<Self> {
        if files.symlinked {
            return None;
        }
        let settings = settings?;
        let mut jsx_standard = settings.jsx_standard && outside_standard?;
        for (path, source, tree_root, _) in base.parsed() {
            let name = path.rsplit('/').next().unwrap_or(path);
            let lang = source_language(name).flatten();
            let specifiers = lang.is_some_and(|lang| checks_specifiers(lang, settings.allow_js));
            jsx_standard &= audit_source(&files.root, path, source, tree_root, &settings.packages)?
                && !(specifiers
                    && reaches_outside(
                        &files.root,
                        path,
                        tree_root,
                        source,
                        lang == Some(LangId::Javascript),
                        &|specifier| settings.packages.bare_reaches_outside(specifier),
                    ));
        }
        Some(Self {
            jsx_standard,
            globals: RefCell::new(BTreeMap::new()),
        })
    }

    /// 設定・依存・リポジトリのソースが、JSX の型環境を差し替えていないか。
    pub(super) fn jsx_standard(&self) -> bool {
        self.jsx_standard
    }

    /// `name` を大域の型・値としてリポジトリのどこかで宣言しているか (`declare global` の中、
    /// module でない TS ファイルのトップレベルなど)。基本の集合の外は、大域の宣言を持ち得る TS の
    /// ファイルのうち名前が現れるものだけを解析する。確かめられなければ true。
    pub(super) fn declares_global(
        &self,
        base: &RemovalInventory,
        outside: &OutsideScan,
        budget: &Budget,
        name: &str,
    ) -> bool {
        if let Some(&declared) = self.globals.borrow().get(name) {
            return declared;
        }
        let declared = scan_globals(base, outside, budget, name);
        self.globals.borrow_mut().insert(name.to_owned(), declared);
        declared
    }
}

/// React の `memo()` で包んだ関数部品の証明 (Issue #266) が頼る、解析するディレクトリの全体の
/// 監査。在庫を作らずに全ての TS / JS 系のソースを流して読み、監査の印があるものだけを解析する
/// ので、ファイル数に上限を掛けない (読む量と解析する量にだけ上限がある)。設定・依存・ソースが
/// `react` の解決先と React・JSX の型を差し替えていないこと ([`RepositoryAudit`] の `jsx_standard`
/// と同じ条件) を確かめる。
pub(super) struct ReactEnvironment {
    root: PathBuf,
    packages: PackageBoundary,
    config_files: BTreeSet<PathBuf>,
    allow_js: bool,
}

impl ReactEnvironment {
    /// 差し替えが無ければ Some。先を監査できない symlink、読めない設定・package.json、解析できない
    /// ソースがあるとき、読む量か解析する量の上限を超えるときも None。
    pub(super) fn load(dir: &str) -> Option<Self> {
        let mut files = RepositoryFiles::walk(dir, &HashSet::new())?;
        if files.symlinked {
            return None;
        }
        let settings = Settings::load(&files)?;
        if !settings.jsx_standard {
            return None;
        }
        let sources = std::mem::take(&mut files.outside);
        let budget = Budget::new(REACT_ENVIRONMENT_LIMITS);
        if !audit_sources(
            &files.root,
            &sources,
            &settings.packages,
            settings.allow_js,
            &budget,
        )? {
            return None;
        }
        Some(Self {
            root: files.root,
            packages: settings.packages,
            config_files: settings.config_files,
            allow_js: settings.allow_js,
        })
    }

    /// `relative` (`dir` 相対、`/` 区切り) の名前が、`react` の解決先か入る依存を決めるファイル
    /// (tsconfig / jsconfig、package.json、パッケージマネージャの設定、ロックファイル) のものか。
    pub(super) fn names_environment(relative: &str) -> bool {
        let name = relative.rsplit('/').next().unwrap_or(relative);
        file_role(name).is_some() || DEPENDENCY_STATE_NAMES.contains(&name)
    }

    /// `relative` が [`Self::names_environment`] の名前か、読んだ設定 (`extends`・`references` の先
    /// を含む) か。
    pub(super) fn is_environment_path(&self, relative: &str) -> bool {
        if Self::names_environment(relative) {
            return true;
        }
        let path = self.root.join(relative);
        self.config_files.contains(&path)
            || path
                .canonicalize()
                .is_ok_and(|canonical| self.config_files.contains(&canonical))
    }

    /// 変更前の `relative` の中身 (`read` で読む) が、React の import 元と JSX の型環境を差し替えて
    /// いないか (いまのソースと同じ監査)。TS / JS 系でないファイルと埋め込みのソースは、いまの
    /// ソースの監査と同じく見ない (読まない)。読めない・解析できなければ false。
    pub(super) fn keeps_standard(
        &self,
        relative: &str,
        read: impl FnOnce() -> Option<Vec<u8>>,
    ) -> bool {
        let name = relative.rsplit('/').next().unwrap_or(relative);
        let Some(Some(lang)) = source_language(name) else {
            return true;
        };
        let Some(source) = read() else {
            return false;
        };
        let Ok(tree) = crate::engine::parser::parse_source(&source, lang) else {
            return false;
        };
        let tree_root = tree.root_node();
        !tree_root.has_error()
            && audit_source(&self.root, relative, &source, tree_root, &self.packages) == Some(true)
            && !(checks_specifiers(lang, self.allow_js)
                && reaches_outside(
                    &self.root,
                    relative,
                    tree_root,
                    &source,
                    lang == LangId::Javascript,
                    &|specifier| self.packages.bare_reaches_outside(specifier),
                ))
    }
}

/// tsconfig / jsconfig の `extends`・`references` の鎖も含めて読み、JSX の設定と `paths`・
/// `baseUrl`・`types` を集める。
struct ConfigScan {
    root: PathBuf,
    bytes: usize,
    checked: BTreeSet<PathBuf>,
    standard: bool,
    path_keys: Vec<String>,
    base_dirs: Vec<PathBuf>,
    /// `paths` のキーと置き換え先。
    path_mappings: Vec<PathMapping>,
    /// `compilerOptions.types` のパッケージの名前。
    types: Vec<String>,
    /// どれかの設定が `allowJs`・`checkJs` を有効にした (jsconfig は既定で有効)。
    allow_js: bool,
    /// 読んだ設定 (正規化したパス) → プロジェクトとしての実効値を求める材料。
    configs: BTreeMap<PathBuf, ConfigEntry>,
    /// `${configDir}` で始まる `files`・`include`・`typeRoots`・相対の `types` と、書いた設定
    /// (正規化したパス)。使う設定で位置が変わるので、読み終えてから、書いた設定とそれを `extends`
    /// で引き継ぐ設定のディレクトリで確かめる。
    deferred: Vec<(PathBuf, &'static str, String)>,
    /// `${configDir}` を含む `baseUrl` と、書いた設定。読み終えてから同じように解く。
    deferred_bases: Vec<(PathBuf, String)>,
}

/// 設定 1 つの、`extends` で引き継がれる値。
#[derive(Default)]
struct ConfigEntry {
    directory: PathBuf,
    /// `baseUrl` (書いたとおりの値)。
    base: Option<String>,
    /// `paths` の置き換え先 (書いたとおりの値。`paths` を書けば空でも上書きする)。
    paths: Option<Vec<String>>,
    /// `extends` の先の設定 (正規化したパス)。後のものが前のものを上書きする。
    parents: Vec<PathBuf>,
    /// 書いた値 (`baseUrl`・`typeRoots`・`types`・`files`・`include`)。書けば引き継いだ値を上書きする。
    keys: BTreeSet<&'static str>,
}

/// 設定をプロジェクトとして使うときの、`baseUrl` (値と、書いた設定のディレクトリ) と、`paths` を
/// 書いた設定 (正規化したパス)。
#[derive(Default)]
struct EffectivePaths<'a> {
    base: Option<(&'a str, &'a Path)>,
    paths: Option<&'a Path>,
}

/// tsconfig のパスの値で、使う設定のディレクトリに置き換わる変数。
const CONFIG_DIR: &str = "${configDir}";

/// `${configDir}` で始まるパスは `project` (使う設定のディレクトリ) から、ほかは `base` から解く。
/// 途中の `${configDir}` と `\` を含むものは解けないので None。
fn config_path(base: &Path, project: &Path, path: &str) -> Option<PathBuf> {
    if path.contains('\\') {
        return None;
    }
    match path.strip_prefix(CONFIG_DIR) {
        Some(rest) => {
            (!rest.contains(CONFIG_DIR)).then(|| project.join(rest.trim_start_matches('/')))
        }
        None => (!path.contains(CONFIG_DIR)).then(|| base.join(path)),
    }
}

/// tsconfig の `paths` の 1 項目。置き換え先は、`baseUrl` があればそこから、無ければ設定の
/// ディレクトリから解決される。
#[derive(Clone)]
struct PathMapping {
    key: String,
    patterns: Vec<String>,
    directory: PathBuf,
    /// `paths` を書いた設定 (正規化したパス)。
    config: PathBuf,
    /// この `paths` を実効値として使う設定ごとの、置き換え先を解く位置 (実効の `baseUrl` か、
    /// `paths` を書いた設定のディレクトリ) と、`${configDir}` を置き換える設定のディレクトリ。
    uses: Vec<(PathBuf, PathBuf)>,
}

impl PathMapping {
    /// `specifier` (キーに当たるもの) を置き換えたパス。キーの `*` が 2 個以上なら None。
    fn substitute(&self, specifier: &str) -> Option<Vec<String>> {
        let captured = match self.key.split_once('*') {
            None => "",
            Some((_, suffix)) if suffix.contains('*') => return None,
            Some((prefix, suffix)) => specifier.strip_prefix(prefix)?.strip_suffix(suffix)?,
        };
        Some(
            self.patterns
                .iter()
                .map(|pattern| pattern.replacen('*', captured, 1))
                .collect(),
        )
    }
}

impl ConfigScan {
    fn new(root: PathBuf) -> Self {
        Self {
            root,
            bytes: 0,
            checked: BTreeSet::new(),
            standard: true,
            path_keys: Vec::new(),
            base_dirs: Vec::new(),
            path_mappings: Vec::new(),
            types: Vec::new(),
            allow_js: false,
            configs: BTreeMap::new(),
            deferred: Vec::new(),
            deferred_bases: Vec::new(),
        }
    }

    /// `directory` の tsconfig.json / jsconfig.json を読み、どちらかがあれば true。
    fn scan_project_directory(&mut self, directory: &Path) -> Option<bool> {
        let mut found = false;
        for name in PROJECT_CONFIG_NAMES {
            let path = directory.join(name);
            if exists(&path)? {
                self.scan(&path, 0)?;
                found = true;
            }
        }
        Some(found)
    }

    /// 読めない・解釈できない設定、`dir` の外・パッケージを指す `extends`・`references`、`dir` の
    /// 外や node_modules のファイルを取り込む `files`・`include`・`typeRoots` は None。
    fn scan(&mut self, path: &Path, depth: usize) -> Option<()> {
        if depth > MAX_CHAIN_DEPTH {
            return None;
        }
        let canonical = regular_file(path)?;
        if !self.checked.insert(canonical.clone()) {
            return Some(());
        }
        let source = read_utf8_file_limited(path, MAX_CONFIG_BYTES)?;
        self.bytes = self.bytes.checked_add(source.len())?;
        if self.bytes > MAX_CONFIG_TOTAL_BYTES {
            return None;
        }
        let bytes = crate::commands::api_changes::jsonc::json_bytes(&source)?;
        let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        let config = value.as_object()?;
        let directory = canonical.parent()?;
        let mut entry = ConfigEntry {
            directory: directory.to_path_buf(),
            ..ConfigEntry::default()
        };
        // jsconfig は、`allowJs` を書かなければ有効にする。
        let jsconfig = canonical
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("jsconfig"));
        let explicit_allow_js = config
            .get("compilerOptions")
            .and_then(|options| options.get("allowJs"))
            .map(|value| value.as_bool() != Some(false));
        if jsconfig && explicit_allow_js != Some(false) {
            self.allow_js = true;
        }
        if let Some(options) = config.get("compilerOptions") {
            let options = options.as_object()?;
            if options.contains_key("jsxFactory")
                || options.contains_key("jsxFragmentFactory")
                || options
                    .get("jsxImportSource")
                    .is_some_and(|source| source.as_str() != Some("react"))
            {
                self.standard = false;
            }
            // moduleSuffixes と rootDirs は、相対の import と paths の解決先を変える。この証明の
            // 解決器では解けないので、設定を読めないものとして扱う。
            if let Some(suffixes) = options.get("moduleSuffixes") {
                let suffixes = suffixes.as_array()?;
                if !(suffixes.is_empty()
                    || (suffixes.len() == 1 && suffixes[0].as_str() == Some("")))
                {
                    return None;
                }
            }
            if options
                .get("rootDirs")
                .is_some_and(|roots| roots.as_array().is_none_or(|roots| roots.len() > 1))
            {
                return None;
            }
            // allowJs・checkJs で、JS 系のファイルの import も TypeScript のプログラムに効く。
            if ["allowJs", "checkJs"].iter().any(|key| {
                options
                    .get(*key)
                    .is_some_and(|value| value.as_bool() != Some(false))
            }) {
                self.allow_js = true;
            }
            if let Some(paths) = options.get("paths") {
                let entry_paths = entry.paths.insert(Vec::new());
                for (key, patterns) in paths.as_object()? {
                    // `react/jsx-runtime` などの解決先を、`paths` で差し替えない。
                    self.standard &= !maps_react(key);
                    self.path_keys.push(key.clone());
                    let patterns = patterns
                        .as_array()?
                        .iter()
                        .map(|pattern| pattern.as_str().map(str::to_owned))
                        .collect::<Option<Vec<_>>>()?;
                    entry_paths.extend(patterns.iter().cloned());
                    self.path_mappings.push(PathMapping {
                        key: key.clone(),
                        patterns,
                        directory: directory.to_path_buf(),
                        config: canonical.clone(),
                        uses: Vec::new(),
                    });
                }
            }
            for key in ["baseUrl", "typeRoots", "types"] {
                if options.contains_key(key) {
                    entry.keys.insert(key);
                }
            }
            if let Some(base) = options.get("baseUrl") {
                let raw = base.as_str()?;
                entry.base = Some(raw.to_owned());
                // TypeScript は `\` を区切りとして読むが、ここでは同じに解けない (Unix では名前の
                // 一部になる)。
                if raw.contains('\\') {
                    self.standard = false;
                }
                if raw.contains(CONFIG_DIR) {
                    self.deferred_bases
                        .push((canonical.clone(), raw.to_owned()));
                } else {
                    let base = directory.join(raw);
                    self.standard &= !has_local_module(&base, "react")?;
                    self.base_dirs.push(base);
                }
            }
            // `typeRoots` の下のパッケージは、import しなくても大域の型として取り込まれる。
            if let Some(roots) = options.get("typeRoots") {
                for root in roots.as_array()? {
                    let root = root.as_str()?;
                    if root.contains(CONFIG_DIR) {
                        self.deferred
                            .push((canonical.clone(), "typeRoots", root.to_owned()));
                    } else {
                        self.type_root(directory, root)?;
                    }
                }
            }
            if let Some(types) = options.get("types") {
                for name in types.as_array()? {
                    let name = name.as_str()?;
                    if name.starts_with(['.', '/']) || name.starts_with(CONFIG_DIR) {
                        self.included_or_deferred(&canonical, "types", directory, name)?;
                    } else {
                        self.types.push(name.to_owned());
                    }
                }
            }
        }
        // `files`・`include` (どちらも無ければ設定のディレクトリの全体) が取り込むファイル。
        let mut explicit = false;
        for field in ["files", "include"] {
            if let Some(entries) = config.get(field) {
                explicit = true;
                entry.keys.insert(field);
                for included in entries.as_array()? {
                    self.included_or_deferred(&canonical, field, directory, included.as_str()?)?;
                }
            }
        }
        if !explicit {
            self.included(directory, "")?;
        }
        let parents: Vec<&str> = match config.get("extends") {
            None => Vec::new(),
            Some(serde_json::Value::String(parent)) => vec![parent.as_str()],
            Some(serde_json::Value::Array(parents)) => parents
                .iter()
                .map(serde_json::Value::as_str)
                .collect::<Option<_>>()?,
            Some(_) => return None,
        };
        for parent in parents {
            let parent = self.extended_path(&canonical, parent)?;
            entry.parents.push(parent.canonicalize().ok()?);
            self.scan(&parent, depth + 1)?;
        }
        self.configs.insert(canonical.clone(), entry);
        // solution 形式の設定では、ソースに効くのは参照先のプロジェクトの設定になる。
        if let Some(references) = config.get("references") {
            for reference in references.as_array()? {
                let path = reference.as_object()?.get("path")?.as_str()?;
                let referenced = self.referenced_path(&canonical, path)?;
                self.scan(&referenced, depth + 1)?;
            }
        }
        Some(())
    }

    /// `files` のパスか `include` の glob が、解析するディレクトリの中の node_modules の外に
    /// 収まるか。glob の `*`・`**`・`?` は 1 区切りの中にしか当たらないので、字句の上で `..` を
    /// 畳んだパスの位置で決まる。
    fn included(&self, directory: &Path, pattern: &str) -> Option<()> {
        let inner = inner_path(&self.root, directory, pattern)?;
        (!has_node_modules(&inner)).then_some(())
    }

    /// [`Self::included`] と同じ。`${configDir}` を含むものは、読み終えてから確かめる。
    fn included_or_deferred(
        &mut self,
        config: &Path,
        key: &'static str,
        directory: &Path,
        pattern: &str,
    ) -> Option<()> {
        if pattern.contains(CONFIG_DIR) {
            self.deferred
                .push((config.to_path_buf(), key, pattern.to_owned()));
            return Some(());
        }
        self.included(directory, pattern)
    }

    /// 設定 `path` をプロジェクトとして使うときに、値 `key` を書いた設定 (自分か、`extends` の先の
    /// うち後のもの)。どれも書かなければ `Some(None)`。鎖が深すぎる・読んでいない設定は None。
    fn effective_source(&self, path: &Path, key: &str, depth: usize) -> Option<Option<&Path>> {
        if depth > MAX_CHAIN_DEPTH {
            return None;
        }
        let (own, entry) = self.configs.get_key_value(path)?;
        if entry.keys.contains(key) {
            return Some(Some(own.as_path()));
        }
        let mut source = None;
        for parent in &entry.parents {
            if let Some(found) = self.effective_source(parent, key, depth + 1)? {
                source = Some(found);
            }
        }
        Some(source)
    }

    /// 設定 `config` の値 `key` を実効値として使う設定 (決められない設定も含める)。
    fn inheriting(&self, config: &Path, key: &str) -> Vec<&ConfigEntry> {
        self.configs
            .iter()
            .filter(|(path, _)| {
                self.effective_source(path, key, 0)
                    .is_none_or(|source| source == Some(config))
            })
            .map(|(_, entry)| entry)
            .collect()
    }

    /// `${configDir}` を含む `baseUrl` を、書いた設定とそれを引き継ぐ設定のディレクトリで解き、
    /// ローカルの `react` を探して、依存の判定に使う `baseUrl` に加える。解けなければ標準でない。
    fn expand_deferred_bases(&mut self) -> Option<()> {
        let mut bases = Vec::new();
        for (config, raw) in &self.deferred_bases {
            let directory = self.configs.get(config)?.directory.clone();
            for project in self.inheriting(config, "baseUrl") {
                bases.push(config_path(&directory, &project.directory, raw));
            }
        }
        for base in bases {
            match base {
                Some(base) => {
                    self.standard &= !has_local_module(&base, "react")?;
                    self.base_dirs.push(base);
                }
                None => self.standard = false,
            }
        }
        Some(())
    }

    /// `typeRoots` の項目。既定と同じ `node_modules/@types` は、registry の型として依存の判定に
    /// 任せる。外や node_modules (ほかの場所) を指せば None。
    fn type_root(&self, directory: &Path, entry: &str) -> Option<()> {
        let inner = inner_path(&self.root, directory, entry)?;
        let modules = inner
            .components()
            .filter(|component| component.as_os_str() == "node_modules")
            .count();
        (modules == 0 || (modules == 1 && inner.ends_with("node_modules/@types"))).then_some(())
    }

    /// `${configDir}` で始まる `files`・`include`・`typeRoots`・相対の `types` が、どの設定から
    /// 使われても解析するディレクトリの中の node_modules の外 (`typeRoots` は既定と同じ
    /// `node_modules/@types` も) に収まるか。途中の `${configDir}` は解けないので false。
    fn deferred_stay_inside(&self) -> bool {
        self.deferred.iter().all(|(config, key, pattern)| {
            self.inheriting(config, key).into_iter().all(|project| {
                config_path(&project.directory, &project.directory, pattern)
                    .and_then(|path| normalize_lexically(&path))
                    .and_then(|path| path.strip_prefix(&self.root).ok().map(Path::to_path_buf))
                    .is_some_and(|inner| {
                        let modules = inner
                            .components()
                            .filter(|component| component.as_os_str() == "node_modules")
                            .count();
                        modules == 0 || (modules == 1 && inner.ends_with("node_modules/@types"))
                    })
            })
        })
    }

    /// 設定 `path` をプロジェクトとして使うときの `baseUrl` と `paths`。`extends` は後の設定が前の
    /// 設定を上書きし、自分の値が全てを上書きする。鎖が深すぎる・読んでいない設定を指すときは None。
    fn effective(&self, path: &Path, depth: usize) -> Option<EffectivePaths<'_>> {
        if depth > MAX_CHAIN_DEPTH {
            return None;
        }
        let (own, entry) = self.configs.get_key_value(path)?;
        let mut effective = EffectivePaths::default();
        for parent in &entry.parents {
            let inherited = self.effective(parent, depth + 1)?;
            effective.base = inherited.base.or(effective.base);
            effective.paths = inherited.paths.or(effective.paths);
        }
        if let Some(base) = &entry.base {
            effective.base = Some((base.as_str(), entry.directory.as_path()));
        }
        if entry.paths.is_some() {
            effective.paths = Some(own.as_path());
        }
        Some(effective)
    }

    /// 読んだ設定のそれぞれをプロジェクトとして使うときの実効の `baseUrl` と `paths` から、`paths` の
    /// 項目ごとに、置き換え先を解く位置 (実効の `baseUrl` か `paths` を書いた設定のディレクトリ) と
    /// `${configDir}` を置き換える設定のディレクトリを記録する。実効値を求められない・`baseUrl` を
    /// 解けない設定があれば false。
    fn resolve_mapping_uses(&mut self) -> bool {
        let mut uses = Vec::new();
        let mut resolved = true;
        for (path, project) in &self.configs {
            let Some(effective) = self.effective(path, 0) else {
                resolved = false;
                continue;
            };
            let Some(source) = effective.paths else {
                continue;
            };
            let base = match effective.base {
                Some((raw, directory)) => config_path(directory, &project.directory, raw),
                None => self
                    .configs
                    .get(source)
                    .map(|entry| entry.directory.clone()),
            };
            match base {
                Some(base) => uses.push((source.to_path_buf(), base, project.directory.clone())),
                None => resolved = false,
            }
        }
        for (source, base, project) in uses {
            for mapping in self
                .path_mappings
                .iter_mut()
                .filter(|mapping| mapping.config == source)
            {
                mapping.uses.push((base.clone(), project.clone()));
            }
        }
        resolved
    }

    /// `paths` の置き換え先と `baseUrl` が、どの設定から使われても解析するディレクトリの中の
    /// node_modules の外に収まるか。外のファイルは監査せず、node_modules の中は registry の名前を
    /// 通らずに React の型を指し得る。置き換え先は、使う設定ごとの位置
    /// ([`Self::resolve_mapping_uses`]) から、`*` に区切りの無い名前と空を入れて解く (実際の捕獲は
    /// 指定子の側で [`PackageBoundary::bare_reaches_outside`] が解く)。
    fn mappings_stay_inside(&self) -> bool {
        let inside = |path: &Path| {
            normalize_lexically(path).is_some_and(|path| {
                path.strip_prefix(&self.root)
                    .is_ok_and(|inner| !has_node_modules(inner))
            })
        };
        self.base_dirs.iter().all(|base| inside(base))
            && self.path_mappings.iter().all(|mapping| {
                mapping.uses.iter().all(|(base, project)| {
                    mapping.patterns.iter().all(|pattern| {
                        pattern_stays_inside(pattern, |target| {
                            config_path(base, project, target).is_some_and(|target| inside(&target))
                        })
                    })
                })
            })
    }

    /// `references` の `path` (設定ファイルか、tsconfig.json を持つディレクトリ) を解決する。
    fn referenced_path(&self, config: &Path, path: &str) -> Option<PathBuf> {
        if path.contains('\\') {
            return None;
        }
        let target = config.parent()?.join(path);
        let file = if target.is_dir() {
            target.join("tsconfig.json")
        } else {
            target
        };
        let canonical = file.canonicalize().ok()?;
        canonical.starts_with(&self.root).then_some(file)
    }

    /// `extends` の相対パスを、TypeScript と同じく拡張子の補完を含めて解決する。パッケージ名や
    /// 絶対パス、`dir` の外を指す設定は読まない。
    fn extended_path(&self, config: &Path, parent: &str) -> Option<PathBuf> {
        if !(parent.starts_with("./") || parent.starts_with("../")) || parent.contains('\\') {
            return None;
        }
        let base = config.parent()?.join(parent);
        let path = if base.is_file() || parent.ends_with(".json") {
            base
        } else {
            PathBuf::from(format!("{}.json", base.to_str()?))
        };
        let canonical = path.canonicalize().ok()?;
        canonical.starts_with(&self.root).then_some(path)
    }
}

/// `directory` から `path` を解決し、解析するディレクトリ `root` からの相対パスにする。外なら None。
fn inner_path(root: &Path, directory: &Path, path: &str) -> Option<PathBuf> {
    if path.contains('\\') {
        return None;
    }
    let joined = normalize_lexically(&directory.join(path))?;
    Some(joined.strip_prefix(root).ok()?.to_path_buf())
}

fn has_node_modules(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str() == "node_modules")
}

/// `path` があるか。読めない (権限など) ときは None。
fn exists(path: &Path) -> Option<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Some(true),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Some(false)
        }
        Err(_) => None,
    }
}

/// symlink でない通常のファイルなら、その正規化したパス。
fn regular_file(path: &Path) -> Option<PathBuf> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return None;
    }
    path.canonicalize().ok()
}

/// TypeScript の `paths` のキー (`*` は 1 個まで) が `specifier` に当たるか。`*` が 2 個以上の
/// キーは解釈できないので、どの指定子にも当たるものとみなす。
fn path_key_matches(key: &str, specifier: &str) -> bool {
    match key.split_once('*') {
        None => key == specifier,
        Some((_, suffix)) if suffix.contains('*') => true,
        Some((prefix, suffix)) => {
            specifier.len() >= prefix.len() + suffix.len()
                && specifier.starts_with(prefix)
                && specifier.ends_with(suffix)
        }
    }
}

/// `*` を含み得るパターン (`paths` の置き換え先、package.json の入口) が、どんな捕獲でも外へ
/// 出ないか (`resolve` で確かめる)。捕獲の `..` と `.`・空の区切りは指定子の側で数えるので、`*` に
/// 区切りの無い名前と空を入れて解く。`*` を含む区切りの固定の部分が `.` だけで、捕獲と合わせて
/// `..` になり得るもの (`.*`・`*.` など) と、`*` が 2 つ以上のものは拒む。
fn pattern_stays_inside(pattern: &str, resolve: impl Fn(&str) -> bool) -> bool {
    if pattern.matches('*').count() > 1 {
        return false;
    }
    if let Some(segment) = pattern.split('/').find(|segment| segment.contains('*')) {
        let (prefix, suffix) = segment.split_once('*').unwrap_or((segment, ""));
        let dots = |part: &str| part.chars().all(|character| character == '.');
        if !(prefix.is_empty() && suffix.is_empty())
            && dots(prefix)
            && dots(suffix)
            && prefix.len() + suffix.len() <= 2
        {
            return false;
        }
    }
    ["_", ""]
        .iter()
        .all(|capture| resolve(&pattern.replace('*', capture)))
}

/// package.json の入口 `target` (`directory` からの相対のパス) が、`root` の中を指すか
/// ([`pattern_stays_inside`])。`\` を含むものは解かない。
fn entry_inside(root: &Path, directory: &Path, target: &str) -> bool {
    !target.contains('\\')
        && pattern_stays_inside(target, |target| {
            normalize_lexically(&directory.join(target)).is_some_and(|path| path.starts_with(root))
        })
}

/// `paths` のキーやモジュール宣言の名前が、React の import 元 (`react` とその配下。
/// `react/jsx-runtime` を含む) に当たり得るか。
fn maps_react(key: &str) -> bool {
    match key.split_once('*') {
        None => key == "react" || key.starts_with("react/"),
        Some((_, suffix)) if suffix.contains('*') => true,
        Some((prefix, _)) => {
            REACT_RUNTIME
                .iter()
                .any(|specifier| path_key_matches(key, specifier))
                || prefix.starts_with("react/")
                || "react/".starts_with(prefix)
        }
    }
}

/// `base` の下に、`name` として解決され得るローカルのファイルかディレクトリがあるか。
fn has_local_module(base: &Path, name: &str) -> Option<bool> {
    for extension in MODULE_FILE_EXTENSIONS {
        if exists(&base.join(format!("{name}{extension}")))? {
            return Some(true);
        }
    }
    Some(false)
}

/// モジュールのパスを解いた結果。
pub(super) enum ModuleResolution {
    /// `target` に当たり得る。
    Target,
    /// `target` 以外のファイルに解決される。
    Elsewhere,
    /// package.json のあるディレクトリ。types・typings・main・exports で、中のどのファイルも
    /// 指し得るので、解決先を決められない。
    PackageDirectory,
    /// ファイルもディレクトリも無い。
    Missing,
}

/// `joined` (絶対パス) をモジュールとして解き、`target` (`dir` 相対) に当たり得るかを決める。
/// TypeScript と同じく、ファイル (拡張子の補完を含む) をディレクトリより先に探す。
/// `directory_import` (`./`・`.`・`..` で終わる指定子) なら、ディレクトリだけを探す。
pub(super) fn resolve_module(
    root: &Path,
    joined: &Path,
    directory_import: bool,
    target: &str,
) -> ModuleResolution {
    let is_target = |candidate: &PathBuf| {
        candidate
            .strip_prefix(root)
            .ok()
            .and_then(|inner| inner.to_str())
            .is_some_and(|inner| normalize_workspace_separators(inner) == target)
    };
    let is_file =
        |candidate: &PathBuf| std::fs::metadata(candidate).is_ok_and(|metadata| metadata.is_file());
    let decide = |existing: Vec<PathBuf>| {
        if existing.iter().any(is_target) {
            Some(ModuleResolution::Target)
        } else if existing.is_empty() {
            None
        } else {
            Some(ModuleResolution::Elsewhere)
        }
    };
    if !directory_import
        && let Some(resolution) = decide(
            file_candidates(joined)
                .into_iter()
                .filter(is_file)
                .collect(),
        )
    {
        return resolution;
    }
    if is_file(&joined.join("package.json")) {
        return ModuleResolution::PackageDirectory;
    }
    decide(
        RESOLVED_EXTENSIONS
            .iter()
            .map(|extension| joined.join(format!("index{extension}")))
            .filter(is_file)
            .collect(),
    )
    .unwrap_or(ModuleResolution::Missing)
}

/// package.json の依存の指定が、どこから中身を持ってくるか。
#[derive(Debug, PartialEq, Eq)]
enum SpecSource {
    Registry,
    /// `npm:` の別名 (registry の別のパッケージ)。
    Alias,
    /// リポジトリの中のパッケージ (`workspace:`・`patch:`・中のディレクトリを指す `file:` など)。
    Repository,
    /// リポジトリの外の中身 (git・URL・外のパス・tarball・`catalog:` など)。
    Foreign,
}

fn spec_source(spec: &str, manifest_dir: &Path, root: &Path) -> SpecSource {
    let spec = spec.trim();
    // npm の overrides の `$名前` は、読むときに参照先の指定へ置き換える。残ったものは決められない。
    if spec.starts_with('$') {
        return SpecSource::Foreign;
    }
    if spec.starts_with("npm:") {
        return SpecSource::Alias;
    }
    if registry_spec(spec) {
        return SpecSource::Registry;
    }
    if spec.starts_with("workspace:") || spec.starts_with("patch:") {
        return SpecSource::Repository;
    }
    let path = ["file:", "link:", "portal:"]
        .iter()
        .find_map(|prefix| spec.strip_prefix(prefix))
        .or_else(|| spec.starts_with(['.', '/']).then_some(spec));
    let inside = path.is_some_and(|path| {
        !path.starts_with('~')
            && inner_path(root, manifest_dir, path).is_some_and(|inner| !has_node_modules(&inner))
            && std::fs::symlink_metadata(manifest_dir.join(path))
                .is_ok_and(|metadata| metadata.is_dir())
    });
    if inside {
        SpecSource::Repository
    } else {
        SpecSource::Foreign
    }
}

/// overrides・resolutions・patchedDependencies のキー (npm の `name`・`name@range`、pnpm の
/// 親子の選択子 `parent>child`、yarn のパス `**/parent/child`) に現れる、全てのパッケージ名。
/// 範囲の中の `>`・`>=`・`<` は区切りにしない (pnpm と同じく、空白・`|`・`@` の直後の `>` は範囲の
/// 一部とみなす)。解釈できない・曖昧なキーは None。
fn override_names(key: &str) -> Option<Vec<String>> {
    let bytes = key.as_bytes();
    let delimiters: Vec<usize> = (1..bytes.len())
        .filter(|&at| bytes[at] == b'>' && !matches!(bytes[at - 1], b' ' | b'|' | b'@'))
        .collect();
    let parts: Vec<&str> = match delimiters.as_slice() {
        [] => vec![key],
        [at] => vec![&key[..*at], &key[at + 1..]],
        _ => return None,
    };
    let name_byte =
        |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~');
    let mut names = Vec::new();
    for part in parts {
        let part = part.as_bytes();
        let mut at = 0;
        loop {
            // yarn のパスの glob (`**/`・`*/`)。
            while part[at..].starts_with(b"**/") || part[at..].starts_with(b"*/") {
                at += if part[at..].starts_with(b"**/") { 3 } else { 2 };
            }
            let start = at;
            if part.get(at) == Some(&b'@') {
                at += 1;
                let scope = at;
                while part.get(at).is_some_and(|byte| name_byte(*byte)) {
                    at += 1;
                }
                if at == scope || part.get(at) != Some(&b'/') {
                    return None;
                }
                at += 1;
            }
            let bare = at;
            while part.get(at).is_some_and(|byte| name_byte(*byte)) {
                at += 1;
            }
            if at == bare {
                return None;
            }
            names.push(std::str::from_utf8(&part[start..at]).ok()?.to_owned());
            match part.get(at) {
                None => break,
                // 版の範囲は、その部分の終わりまで続く。
                Some(b'@') => break,
                Some(b'/') => at += 1,
                Some(_) => return None,
            }
        }
    }
    Some(names)
}

/// package.json の、この証明に要る部分。
struct Manifest {
    external_consumers: bool,
    name: Option<String>,
    workspaces: bool,
    /// 型とモジュールの入口 (`types`・`typings`・`main`・`module`・`exports`・`typesVersions`・
    /// `imports`) の文字列。
    entry_targets: Vec<String>,
    /// `imports` (`#` の指定子の解決先) を持つ。
    imports: bool,
    /// `exports` (名前での自己参照の解決先) を持つ。
    exports: bool,
    /// 依存と overrides・resolutions の、名前と版の指定 (文字列でなければ None)。
    dependencies: Vec<(String, Option<String>)>,
    /// パッチを当てた依存の名前。
    patched: Vec<String>,
}

impl Manifest {
    fn read(path: &Path, bytes: &mut usize) -> Option<Self> {
        regular_file(path)?;
        let source = read_utf8_file_limited(path, MAX_CONFIG_BYTES)?;
        *bytes = bytes.checked_add(source.len())?;
        if *bytes > MAX_CONFIG_TOTAL_BYTES {
            return None;
        }
        let value: serde_json::Value = serde_json::from_str(&source).ok()?;
        let object = value.as_object()?;
        let name = match object.get("name") {
            None => None,
            Some(name) => Some(name.as_str()?.to_owned()),
        };
        let mut entry_targets = Vec::new();
        let mut pending: Vec<&serde_json::Value> = [
            "types",
            "typings",
            "main",
            "module",
            "exports",
            "typesVersions",
            "imports",
        ]
        .iter()
        .filter_map(|field| object.get(*field))
        .collect();
        while let Some(value) = pending.pop() {
            match value {
                serde_json::Value::String(target) => entry_targets.push(target.clone()),
                serde_json::Value::Array(items) => pending.extend(items),
                serde_json::Value::Object(entries) => pending.extend(entries.values()),
                _ => {}
            }
        }
        let mut dependencies = Vec::new();
        for field in [
            "dependencies",
            "devDependencies",
            "peerDependencies",
            "optionalDependencies",
        ] {
            let Some(entries) = object.get(field) else {
                continue;
            };
            for (dependency, spec) in entries.as_object()? {
                dependencies.push((dependency.clone(), spec.as_str().map(str::to_owned)));
            }
        }
        let pnpm = match object.get("pnpm") {
            None => None,
            Some(pnpm) => Some(pnpm.as_object()?),
        };
        // npm の overrides は入れ子の object (`.` は親のパッケージ自身)、yarn の resolutions は
        // パス、pnpm の overrides は `a>b` で、差し替え先を指す。
        let overrides = [
            object.get("overrides"),
            object.get("resolutions"),
            pnpm.and_then(|pnpm| pnpm.get("overrides")),
        ];
        let mut overridden = Vec::new();
        // 入れ子の overrides では、外側のキーの名前も、内側の指定で決まる依存の経路に入る。
        let mut stack: Vec<(&serde_json::Map<String, serde_json::Value>, Vec<String>)> = Vec::new();
        for entries in overrides.into_iter().flatten() {
            stack.push((entries.as_object()?, Vec::new()));
        }
        while let Some((entries, parents)) = stack.pop() {
            for (key, value) in entries {
                let mut names = parents.clone();
                if key == "." {
                    if parents.is_empty() {
                        return None;
                    }
                } else {
                    names.extend(override_names(key)?);
                }
                match value {
                    serde_json::Value::Object(nested) => stack.push((nested, names)),
                    other => {
                        for name in names {
                            overridden.push((name, other.as_str().map(str::to_owned)));
                        }
                    }
                }
            }
        }
        // npm の `$名前` は、同じ package.json の直接の依存の指定を参照する。参照先が無ければ、
        // `$` のまま残して決められないものにする。
        for (target, spec) in overridden {
            let references: Vec<Option<String>> = match spec.as_deref() {
                Some(reference) if reference.starts_with('$') => {
                    let found: Vec<Option<String>> = dependencies
                        .iter()
                        .filter(|(dependency, _)| dependency == &reference[1..])
                        .map(|(_, spec)| spec.clone())
                        .collect();
                    if found.is_empty() { vec![spec] } else { found }
                }
                _ => vec![spec],
            };
            for spec in references {
                dependencies.push((target.clone(), spec));
            }
        }
        let mut patched = Vec::new();
        for entries in [
            object.get("patchedDependencies"),
            pnpm.and_then(|pnpm| pnpm.get("patchedDependencies")),
        ]
        .into_iter()
        .flatten()
        {
            for key in entries.as_object()?.keys() {
                patched.extend(override_names(key)?);
            }
        }
        Some(Self {
            external_consumers: [
                "types",
                "typings",
                "main",
                "module",
                "exports",
                "typesVersions",
                "imports",
            ]
            .iter()
            .any(|field| object.contains_key(*field))
                || (name.is_some()
                    && object.get("private").and_then(serde_json::Value::as_bool) != Some(true)),
            name,
            workspaces: object.contains_key("workspaces"),
            entry_targets,
            imports: object.contains_key("imports"),
            exports: object.contains_key("exports"),
            dependencies,
            patched,
        })
    }
}

/// bare specifier が registry の依存を指すかを決める材料。
pub(super) struct PackageBoundary {
    external_consumers: bool,
    root: PathBuf,
    /// package.json の `name` (ワークスペースのパッケージと、`exports` による自己参照)。
    workspace_names: BTreeSet<String>,
    /// package.json の `name` → そのディレクトリ (`dir` 相対。外の祖先の package.json は None)。
    package_dirs: BTreeMap<String, Vec<Option<String>>>,
    path_keys: Vec<String>,
    path_mappings: Vec<PathMapping>,
    base_dirs: Vec<PathBuf>,
    /// どこかの package.json が registry でない指定で入れる名前。巻き上げで、どのパッケージ
    /// からも解決され得るので、指定した package.json の位置に関係なく registry とは決めない。
    non_registry: BTreeSet<String>,
    /// `npm:` の別名で入れた名前。
    aliased: BTreeSet<String>,
    /// リポジトリの外の中身を、registry を通さずに入れる依存がある。その推移的な依存の型
    /// (`@types/*`) が大域に入り得る。
    foreign: bool,
    /// pnpm-workspace.yaml の catalog の指定 (`catalog:` の参照先)。
    catalogs: Vec<CatalogEntry>,
    /// 解析するディレクトリの中の package.json の、型とモジュールの入口が外を指す。
    entries_escape: bool,
}

/// pnpm-workspace.yaml の catalog の 1 項目。
struct CatalogEntry {
    /// catalog の名前 (`catalog` は `default`)。
    catalog: String,
    name: String,
    spec: Option<String>,
    /// 相対のパスの指定の基点 (pnpm-workspace.yaml のディレクトリ)。
    directory: PathBuf,
}

/// pnpm-workspace.yaml・.yarnrc.yml の、依存の指定を差し替える部分。
struct PackageConfig {
    /// overrides と packageExtensions の依存の、名前と版の指定。
    dependencies: Vec<(String, Option<String>)>,
    /// パッチを当てた依存の名前。
    patched: Vec<String>,
    /// catalog の名前・依存の名前・版の指定。
    catalogs: Vec<(String, String, Option<String>)>,
}

/// YAML の mapping の、文字列のキーと値。null は空。mapping でないかキーが文字列でなければ None。
fn yaml_entries(value: &serde_yaml::Value) -> Option<Vec<(String, &serde_yaml::Value)>> {
    match value {
        serde_yaml::Value::Null => Some(Vec::new()),
        serde_yaml::Value::Mapping(mapping) => mapping
            .iter()
            .map(|(key, value)| Some((key.as_str()?.to_owned(), value)))
            .collect(),
        _ => None,
    }
}

/// 版の指定。文字列と数 (`18` など) は文字列にする。それ以外は None (決められない)。
fn yaml_spec(value: &serde_yaml::Value) -> Option<String> {
    match value {
        serde_yaml::Value::String(text) => Some(text.clone()),
        serde_yaml::Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

impl PackageConfig {
    /// 読めない・解釈できない設定と、pnpmfile の場所を変える指定は None。
    fn read(path: &Path, bytes: &mut usize) -> Option<Self> {
        regular_file(path)?;
        let source = read_utf8_file_limited(path, MAX_CONFIG_BYTES)?;
        *bytes = bytes.checked_add(source.len())?;
        if *bytes > MAX_CONFIG_TOTAL_BYTES {
            return None;
        }
        let value: serde_yaml::Value = serde_yaml::from_str(&source).ok()?;
        let mut config = Self {
            dependencies: Vec::new(),
            patched: Vec::new(),
            catalogs: Vec::new(),
        };
        let fields = yaml_entries(&value)?;
        for (field, value) in fields {
            match field.as_str() {
                // 依存の指定を任意に書き換えられる hook の場所。
                "pnpmfile" | "globalPnpmfile" => return None,
                "overrides" => {
                    for (key, spec) in yaml_entries(value)? {
                        for name in override_names(&key)? {
                            config.dependencies.push((name, yaml_spec(spec)));
                        }
                    }
                }
                "packageExtensions" => {
                    for (_, extension) in yaml_entries(value)? {
                        for (kind, entries) in yaml_entries(extension)? {
                            if !matches!(
                                kind.as_str(),
                                "dependencies" | "optionalDependencies" | "peerDependencies"
                            ) {
                                continue;
                            }
                            for (name, spec) in yaml_entries(entries)? {
                                config.dependencies.push((name, yaml_spec(spec)));
                            }
                        }
                    }
                }
                "patchedDependencies" => {
                    for (key, _) in yaml_entries(value)? {
                        config.patched.extend(override_names(&key)?);
                    }
                }
                "catalog" => {
                    for (name, spec) in yaml_entries(value)? {
                        config
                            .catalogs
                            .push(("default".to_owned(), name, yaml_spec(spec)));
                    }
                }
                "catalogs" => {
                    for (catalog, entries) in yaml_entries(value)? {
                        for (name, spec) in yaml_entries(entries)? {
                            config
                                .catalogs
                                .push((catalog.clone(), name, yaml_spec(spec)));
                        }
                    }
                }
                _ => {}
            }
        }
        Some(config)
    }
}

impl PackageBoundary {
    /// 解析するディレクトリの中の全ての package.json と、外の祖先の package.json を読む。
    /// 読めないものや、外にワークスペースの定義があれば None。
    fn load(files: &RepositoryFiles, configs: &ConfigScan) -> Option<Self> {
        let mut boundary = Self {
            external_consumers: false,
            root: files.root.clone(),
            workspace_names: BTreeSet::new(),
            package_dirs: BTreeMap::new(),
            path_keys: configs.path_keys.clone(),
            path_mappings: configs.path_mappings.clone(),
            base_dirs: configs.base_dirs.clone(),
            non_registry: BTreeSet::new(),
            aliased: BTreeSet::new(),
            foreign: false,
            catalogs: Vec::new(),
            entries_escape: false,
        };
        let mut bytes = 0;
        let mut package_configs = Vec::new();
        for path in &files.package_configs {
            package_configs.push((
                path.parent()?.to_path_buf(),
                PackageConfig::read(path, &mut bytes)?,
            ));
        }
        let mut manifests = Vec::new();
        for path in &files.manifests {
            let directory = path.parent()?.to_path_buf();
            let manifest = Manifest::read(path, &mut bytes)?;
            // 入口は import (ディレクトリの import・`types`・reference) のどれからも使われ得るので、
            // どこにある package.json も確かめる。
            boundary.entries_escape |= !manifest
                .entry_targets
                .iter()
                .all(|target| entry_inside(&files.root, &directory, target));
            manifests.push((directory, manifest));
        }
        // 外の祖先の package.json の依存も、祖先の node_modules から解決され得る。外に
        // ワークスペースの定義や pnpmfile があると、外のパッケージの名前や依存の指定を確かめられ
        // ない。yarn は祖先の .yarnrc.yml も読む。
        for base in [&files.logical, &files.root] {
            for (depth, ancestor) in base.ancestors().enumerate().skip(1) {
                if depth > MAX_ANCESTORS {
                    return None;
                }
                for marker in ["pnpm-workspace.yaml", "lerna.json"]
                    .iter()
                    .chain(PNPMFILE_NAMES.iter())
                {
                    if exists(&ancestor.join(marker))? {
                        return None;
                    }
                }
                let yarnrc = ancestor.join(".yarnrc.yml");
                if exists(&yarnrc)? {
                    package_configs.push((
                        ancestor.to_path_buf(),
                        PackageConfig::read(&yarnrc, &mut bytes)?,
                    ));
                }
                let path = ancestor.join("package.json");
                if exists(&path)? {
                    let manifest = Manifest::read(&path, &mut bytes)?;
                    if manifest.workspaces {
                        return None;
                    }
                    // 祖先の package.json の `imports` は中の `#` の指定子を、`exports` は名前での
                    // 自己参照を、監査しない外のファイルへ解き得る。
                    boundary.entries_escape |=
                        manifest.imports || (manifest.name.is_some() && manifest.exports);
                    manifests.push((ancestor.to_path_buf(), manifest));
                }
            }
        }
        // `catalog:` の参照を解くので、catalog を先に集める。
        for (directory, config) in &package_configs {
            for (catalog, name, spec) in &config.catalogs {
                boundary.catalogs.push(CatalogEntry {
                    catalog: catalog.clone(),
                    name: name.clone(),
                    spec: spec.clone(),
                    directory: directory.clone(),
                });
            }
        }
        for (directory, config) in &package_configs {
            let catalog_entries: Vec<(String, Option<String>)> = config
                .catalogs
                .iter()
                .map(|(_, name, spec)| (name.clone(), spec.clone()))
                .collect();
            boundary.record_dependencies(directory, &config.dependencies, &files.root)?;
            boundary.record_dependencies(directory, &catalog_entries, &files.root)?;
            boundary.non_registry.extend(config.patched.iter().cloned());
        }
        for (directory, manifest) in &manifests {
            boundary.external_consumers |= manifest.external_consumers;
            boundary.record(directory, manifest, &files.root)?;
        }
        Some(boundary)
    }

    fn record(&mut self, directory: &Path, manifest: &Manifest, root: &Path) -> Option<()> {
        self.record_dependencies(directory, &manifest.dependencies, root)?;
        self.non_registry.extend(manifest.patched.iter().cloned());
        if let Some(name) = &manifest.name {
            self.workspace_names.insert(name.clone());
            let inner = directory
                .strip_prefix(root)
                .ok()
                .map(|inner| normalize_workspace_separators(&inner.to_string_lossy()));
            self.package_dirs
                .entry(name.clone())
                .or_default()
                .push(inner);
        }
        Some(())
    }

    /// 依存の名前と版の指定を記録する。pnpm の `catalog:` は、catalog の指定に置き換えて判定する
    /// (見つからなければ決められない)。文字列でない指定があれば None。
    fn record_dependencies(
        &mut self,
        directory: &Path,
        entries: &[(String, Option<String>)],
        root: &Path,
    ) -> Option<()> {
        for (name, spec) in entries {
            let spec = spec.as_deref()?;
            let resolved: Vec<(String, PathBuf)> = match spec.trim().strip_prefix("catalog:") {
                Some(catalog) => {
                    let catalog = if catalog.is_empty() {
                        "default"
                    } else {
                        catalog
                    };
                    let found: Vec<(String, PathBuf)> = self
                        .catalogs
                        .iter()
                        .filter(|entry| entry.catalog == catalog && &entry.name == name)
                        .map(|entry| {
                            (
                                entry.spec.clone().unwrap_or_else(|| "$".to_owned()),
                                entry.directory.clone(),
                            )
                        })
                        .collect();
                    if found.is_empty() {
                        vec![(spec.to_owned(), directory.to_path_buf())]
                    } else {
                        found
                    }
                }
                None => vec![(spec.to_owned(), directory.to_path_buf())],
            };
            for (spec, base) in resolved {
                match spec_source(&spec, &base, root) {
                    SpecSource::Registry => {}
                    SpecSource::Alias => {
                        self.aliased.insert(name.clone());
                    }
                    SpecSource::Repository => {
                        self.non_registry.insert(name.clone());
                    }
                    SpecSource::Foreign => {
                        self.non_registry.insert(name.clone());
                        self.foreign = true;
                    }
                }
            }
        }
        Some(())
    }

    /// bare specifier が、registry から入れた依存を指すか。ワークスペースのパッケージ・パスの
    /// 別名・`baseUrl` の下のローカルのモジュール・どこかの package.json での registry でない
    /// 指定 (`file:`・`workspace:` など) のどれかに当たれば false。
    pub(super) fn is_registry_dependency(&self, specifier: &str) -> bool {
        let Some(name) = package_name(specifier) else {
            return false;
        };
        !(self.workspace_names.contains(name)
            || self.non_registry.contains(name)
            || self
                .path_keys
                .iter()
                .any(|key| path_key_matches(key, specifier))
            || self
                .base_dirs
                .iter()
                .any(|base| has_local_module(base, name) != Some(false)))
    }

    /// 相対でも絶対でもない指定子が、`paths` の別名か `baseUrl` の下の解決で、解析するディレクトリの
    /// 外を指し得るか。別名は実際の捕獲を入れて、使う設定ごとの位置 (実効の `baseUrl` か、`paths` を
    /// 書いた設定のディレクトリ) から解く。`..` を含むものは、パッケージの中からも上へ出得るので
    /// 数える。どの別名にも当たらず `baseUrl` も無ければ、node_modules の依存を指す。
    pub(super) fn bare_reaches_outside(&self, specifier: &str) -> bool {
        if specifier.contains("..") {
            return true;
        }
        let inside = |path: &Path| {
            normalize_lexically(path).is_some_and(|path| path.starts_with(&self.root))
        };
        for mapping in self
            .path_mappings
            .iter()
            .filter(|mapping| path_key_matches(&mapping.key, specifier))
        {
            let Some(targets) = mapping.substitute(specifier) else {
                return true;
            };
            for target in &targets {
                if !mapping.uses.iter().all(|(base, project)| {
                    config_path(base, project, target).is_some_and(|path| inside(&path))
                }) {
                    return true;
                }
            }
        }
        !self
            .base_dirs
            .iter()
            .all(|base| inside(&base.join(specifier)))
    }

    /// bare specifier が、リポジトリの中の `target` (`dir` 相対) に解決され得るか。`paths`・
    /// `baseUrl`・リポジトリの package (名前と自己参照) で解き、registry の依存と決められず解決先も
    /// 確かめられないときも true。
    pub(super) fn bare_may_reach(&self, specifier: &str, target: &str) -> bool {
        let mut mapped = false;
        for mapping in self
            .path_mappings
            .iter()
            .filter(|mapping| path_key_matches(&mapping.key, specifier))
        {
            let Some(paths) = mapping.substitute(specifier) else {
                return true;
            };
            let mut exists = false;
            for path in &paths {
                for base in std::iter::once(&mapping.directory).chain(&self.base_dirs) {
                    let Some(joined) = normalize_lexically(&base.join(path)) else {
                        continue;
                    };
                    match resolve_module(&self.root, &joined, false, target) {
                        ModuleResolution::Target | ModuleResolution::PackageDirectory => {
                            return true;
                        }
                        ModuleResolution::Elsewhere => exists = true,
                        ModuleResolution::Missing => {}
                    }
                }
            }
            // 置き換え先が 1 つも無ければ、解決先を確かめられない。
            if !exists {
                return true;
            }
            mapped = true;
        }
        if mapped {
            return false;
        }
        for base in &self.base_dirs {
            let Some(joined) = normalize_lexically(&base.join(specifier)) else {
                continue;
            };
            match resolve_module(&self.root, &joined, false, target) {
                ModuleResolution::Target | ModuleResolution::PackageDirectory => return true,
                ModuleResolution::Elsewhere => return false,
                ModuleResolution::Missing => {}
            }
        }
        let Some(name) = package_name(specifier) else {
            // `#` の subpath import や URL は、解決先を確かめられない。
            return true;
        };
        if let Some(directories) = self.package_dirs.get(name) {
            // リポジトリの package は、exports で中のどのファイルも公開し得る。外の祖先の package
            // (自己参照) は、解析するディレクトリの全体を含む。
            return directories.iter().any(|directory| {
                directory.as_deref().is_none_or(|directory| {
                    directory.is_empty() || target.starts_with(&format!("{directory}/"))
                })
            });
        }
        !self.is_registry_dependency(specifier)
    }

    /// 解析するディレクトリの中の package.json の型とモジュールの入口 (`types`・`typings`・`main`・
    /// `module`・`exports`・`typesVersions`・`imports`) が、全て中を指すか。外のファイルを入口に
    /// すれば、監査しないファイルが TypeScript のプログラムに入り、React の型を差し替え得る。
    pub(super) fn entries_stay_inside(&self) -> bool {
        !self.entries_escape
    }

    /// 公開 package の入口を持つときは、リポジトリ内の利用だけで閉じたとは証明しない。
    pub(super) fn consumers_are_local(&self) -> bool {
        !self.external_consumers
    }

    /// JSX の証明が頼る React (自動の runtime の `react/jsx-runtime`・`react/jsx-dev-runtime` と、
    /// 型の `@types/react`) が、registry の React (別名でない) から解決されるか。外の中身の依存や
    /// registry でない `@types/*` があれば、大域の JSX の型が変わり得るので false。
    fn standard_react(&self) -> bool {
        !self.foreign
            && !self
                .non_registry
                .iter()
                .any(|name| name.starts_with("@types/"))
            && REACT_RUNTIME
                .iter()
                .chain(&["@types/react"])
                .all(|specifier| self.is_registry_dependency(specifier))
            && !["react", "@types/react"]
                .iter()
                .any(|name| self.aliased.contains(*name))
    }

    /// `compilerOptions.types`・`/// <reference types>` の名前が、registry の型のパッケージ
    /// (`name` か `@types/` の下の対応する名前) を指すか。
    fn registry_types(&self, name: &str) -> bool {
        let types_package = match name.strip_prefix('@') {
            Some(scoped) => format!("@types/{}", scoped.replacen('/', "__", 1)),
            None => format!("@types/{name}"),
        };
        self.is_registry_dependency(name) && self.is_registry_dependency(&types_package)
    }
}

/// `text` に `needle` が ASCII の大文字小文字を区別せずに現れるか。
fn contains_ignore_ascii_case(text: &[u8], needle: &[u8]) -> bool {
    text.windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

/// `JSX` という名前を作る宣言・公開があるか。型別名・interface・namespace・module・変数・
/// import の束縛 (`import JSX = ...` を含む) と、export の名前や別名 (`export { X as JSX }`・
/// `export * as JSX from`)。型の参照 (`JSX.Element`) は宣言ではないので数えない。
pub(super) fn declares_jsx_name(root: Node<'_>, source: &[u8]) -> bool {
    let is_jsx = |node: Node<'_>| node.utf8_text(source).ok() == Some("JSX");
    collect_bindings_named(root, source, "JSX")
        .into_iter()
        .any(|binding| !imports_react_jsx(binding, source))
        || any_node(root, Children::Named, |node| match node.kind() {
            "export_specifier" => ["name", "alias"]
                .iter()
                .any(|field| node.child_by_field_name(field).is_some_and(is_jsx)),
            "namespace_export" => {
                let mut cursor = node.walk();
                node.named_children(&mut cursor).any(is_jsx)
            }
            _ => false,
        })
}

/// `import { JSX } from "react"` (型だけの import も) のように、React の `JSX` の名前空間を同じ名前で
/// 束縛するだけの import か。React 19 の型で `JSX.Element` を書く普通の形で、JSX の型環境を差し替え
/// ない。default・namespace の import や、別の名前を `JSX` に束縛する import は数える。
fn imports_react_jsx(node: Node<'_>, source: &[u8]) -> bool {
    // 束縛の一覧は import 文と、その中の `name` を持つ specifier の両方を返す。
    let mut node = node;
    while node.kind() != "import_statement" {
        match node.parent() {
            Some(parent)
                if matches!(
                    node.kind(),
                    "import_specifier" | "named_imports" | "import_clause"
                ) =>
            {
                node = parent;
            }
            _ => return false,
        }
    }
    if node.kind() != "import_statement"
        || !node
            .child_by_field_name("source")
            .and_then(|specifier| static_specifier(specifier, source))
            .is_some_and(|specifier| REACT_RUNTIME.contains(&specifier))
    {
        return false;
    }
    let mut bindings = 0usize;
    crate::engine::js_scope::visit_import_bindings(node, source, |name| {
        bindings += usize::from(name == "JSX");
        false
    });
    let mut named = 0usize;
    let _ = walk_preorder(node, Children::Named, |child, _| {
        if child.kind() == "import_specifier" {
            let name = child.child_by_field_name("name");
            let local = child.child_by_field_name("alias").or(name);
            let text = |node: Option<Node<'_>>| node.and_then(|node| node.utf8_text(source).ok());
            if text(local) == Some("JSX") && text(name) == Some("JSX") {
                named += 1;
            }
        }
        ControlFlow::<(), Walk>::Continue(Walk::Descend)
    });
    bindings == named
}

/// JSX の型環境を差し替える宣言か。`namespace JSX` (global・module の augmentation の中を含む)
/// と、`@jsx`・`@jsxImportSource`・`@jsxFrag` などの pragma コメント (TypeScript は pragma の名前の
/// 大文字小文字を区別しない)。
pub(super) fn replaces_jsx_environment(node: Node<'_>, source: &[u8]) -> bool {
    if node.kind() == "comment" {
        return node
            .utf8_text(source)
            .is_ok_and(|text| contains_ignore_ascii_case(text.as_bytes(), b"@jsx"));
    }
    if !matches!(node.kind(), "internal_module" | "module") {
        return false;
    }
    let Some(name) = node.child_by_field_name("name") else {
        return false;
    };
    any_node(name, Children::Named, |part| {
        matches!(part.kind(), "identifier" | "property_identifier")
            && part.utf8_text(source).ok() == Some("JSX")
    })
}

/// `relative` が宣言ファイル (`.d.ts`・`.d.mts`・`.d.cts` と、`.d.css.ts` のような任意の拡張子の
/// 宣言) か。宣言ファイルは全体が ambient の文脈なので、`declare` を省いた宣言を書ける。
pub(super) fn is_declaration_file(relative: &str) -> bool {
    let name = relative.rsplit('/').next().unwrap_or(relative);
    [".d.ts", ".d.mts", ".d.cts"]
        .iter()
        .any(|suffix| name.ends_with(suffix))
        || (name.ends_with(".ts") && name.contains(".d."))
}

/// `declare module "react"` など、React の import 元を augmentation で差し替える宣言か。
/// `*` を含む名前 (`"*.svg"` など) は、`react` と JSX の runtime に当たるときだけ数える。
/// `@types/react` と、`node_modules` を通る相対・絶対のパス (`"../node_modules/@types/react"`)
/// も、`react` の import と同じ宣言ファイルを指し得るので数える。
fn augments_react(node: Node<'_>, source: &[u8]) -> bool {
    node.kind() == "module"
        && node
            .child_by_field_name("name")
            .filter(|name| name.kind() == "string")
            .is_some_and(|name| {
                static_specifier(name, source).is_none_or(|module| {
                    if module.contains('*') {
                        REACT_RUNTIME
                            .iter()
                            .any(|specifier| path_key_matches(module, specifier))
                    } else if module.starts_with(['.', '/'])
                        || (module
                            .as_bytes()
                            .first()
                            .is_some_and(u8::is_ascii_alphabetic)
                            && module.as_bytes().get(1) == Some(&b':'))
                    {
                        module
                            .split('/')
                            .any(|segment| segment.eq_ignore_ascii_case("node_modules"))
                    } else {
                        ["react", "@types/react"].iter().any(|package| {
                            module == *package
                                || module
                                    .strip_prefix(package)
                                    .is_some_and(|rest| rest.starts_with('/'))
                        })
                    }
                })
            })
}

/// JavaScript の正規表現の `\s` に当たる文字 (Rust の空白に BOM を足したもの)。
fn js_whitespace(character: char) -> bool {
    character.is_whitespace() || character == '\u{feff}'
}

/// `/// <reference ... />` なら、タグの名前より後ろ (属性の部分)。TypeScript の
/// `^\/\/\/\s*<(\S+)\s.*?\/>` と同じく、タグの名前は大文字小文字を区別しない。
pub(super) fn reference_attributes(text: &str) -> Option<&str> {
    let rest = text
        .strip_prefix("///")?
        .trim_start_matches(js_whitespace)
        .strip_prefix('<')?;
    let end = rest.find(js_whitespace)?;
    if !rest[..end].eq_ignore_ascii_case("reference") {
        return None;
    }
    let attributes = &rest[end..];
    attributes.contains("/>").then_some(attributes)
}

/// 属性 `name` の値。TypeScript の `\s<name>\s*=\s*(?:'([^']*)'|"([^"]*)")` (大文字小文字を
/// 区別しない) と同じく、空白 (タブを含む) で始まる最初の出現を読む。
pub(super) fn reference_attribute<'a>(attributes: &'a str, name: &str) -> Option<&'a str> {
    let mut after_whitespace = false;
    for (index, character) in attributes.char_indices() {
        if after_whitespace
            && attributes
                .get(index..index + name.len())
                .is_some_and(|candidate| candidate.eq_ignore_ascii_case(name))
            && let Some(rest) = attributes[index + name.len()..]
                .trim_start_matches(js_whitespace)
                .strip_prefix('=')
        {
            let rest = rest.trim_start_matches(js_whitespace);
            if let Some(quote) = rest
                .chars()
                .next()
                .filter(|quote| matches!(quote, '"' | '\''))
                && let Some(end) = rest[1..].find(quote)
            {
                return Some(&rest[1..1 + end]);
            }
        }
        after_whitespace = js_whitespace(character);
    }
    None
}

/// reference directive を、TypeScript と同じ優先順 (`no-default-lib` → `types` → `lib` →
/// `path`) で調べる。外や node_modules のファイルを取り込むものは None。既定の lib を外すもの・
/// registry と決められない `types`・取り込む先の無い `path`・読めない属性は Some(false)。
fn audit_reference(
    text: &str,
    directory: &Path,
    root: &Path,
    packages: &PackageBoundary,
) -> Option<bool> {
    let Some(attributes) = reference_attributes(text) else {
        return Some(true);
    };
    let value =
        |name: &str| reference_attribute(attributes, name).filter(|value| !value.is_empty());
    if reference_attribute(attributes, "no-default-lib").is_some() {
        return Some(false);
    }
    if let Some(types) = value("types") {
        if types.starts_with(['.', '/']) {
            let inner = inner_path(root, directory, types)?;
            return (!has_node_modules(&inner)).then_some(true);
        }
        return Some(packages.registry_types(types));
    }
    if value("lib").is_some() {
        return Some(true);
    }
    let Some(path) = value("path") else {
        return Some(false);
    };
    let inner = inner_path(root, directory, path)?;
    if has_node_modules(&inner) {
        return None;
    }
    let target = root.join(&inner);
    let found = target.is_file()
        || REFERENCE_EXTENSIONS.iter().any(|extension| {
            let mut candidate = target.clone().into_os_string();
            candidate.push(extension);
            Path::new(&candidate).is_file()
        });
    Some(found)
}

/// リポジトリの 1 ファイルを監査する。外や node_modules のファイルを取り込む reference があれば
/// None。JSX の名前空間・`JSX` という名前の宣言と公開・`@jsx` の pragma・React の augmentation・
/// registry と決められない型の参照・escape を含む識別子があれば Some(false)。
pub(super) fn audit_source(
    root: &Path,
    relative: &str,
    source: &[u8],
    tree_root: Node<'_>,
    packages: &PackageBoundary,
) -> Option<bool> {
    // どの宣言も、これらの文字列を含まずには書けない (pragma と reference は大文字小文字を
    // 区別しない)。宣言ファイルは `declare` を省いた `module "react" { … }` も書ける。
    let finds = |needle: &[u8]| memchr::memmem::find(source, needle).is_some();
    if !(finds(b"JSX")
        || finds(b"declare")
        || (is_declaration_file(relative) && finds(b"module"))
        || finds(b"\\")
        || contains_ignore_ascii_case(source, b"@jsx")
        || contains_ignore_ascii_case(source, b"<reference"))
    {
        return Some(true);
    }
    let file = root.join(relative);
    let directory = file.parent()?;
    let mut standard = !declares_jsx_name(tree_root, source);
    let outside = walk_preorder(tree_root, Children::All, |node, _| {
        if replaces_jsx_environment(node, source)
            || augments_react(node, source)
            || escaped_identifier(node, source)
        {
            standard = false;
        }
        if node.kind() == "comment" {
            let text = String::from_utf8_lossy(&source[node.byte_range()]);
            match audit_reference(&text, directory, root, packages) {
                Some(trusted) => standard &= trusted,
                None => return ControlFlow::Break(()),
            }
        }
        ControlFlow::Continue(Walk::Descend)
    });
    (!outside.is_break()).then_some(standard)
}

/// module でない TS ファイルのトップレベルと、module の `declare global` の中で `name` を宣言して
/// いるか。escape を含む識別子があれば、確かめられないので true。
fn declares_global_in(path: &str, source: &[u8], root: Node<'_>, name: &str) -> bool {
    let text = String::from_utf8_lossy(source);
    if !contains_js_word(&text, name) && !text.contains('\\') {
        return false;
    }
    if any_node(root, Children::All, |node| escaped_identifier(node, source)) {
        return true;
    }
    let module_extension = Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("mts") || extension.eq_ignore_ascii_case("cts")
        });
    let mut cursor = root.walk();
    let module = module_extension
        || root
            .named_children(&mut cursor)
            .any(|statement| matches!(statement.kind(), "import_statement" | "export_statement"));
    // module の大域の宣言は `declare global` (`declare module "x" { global { ... } }` を含む)
    // の中にしか書けない。`global` と書いたファイルでは、名前の束縛をすべて大域とみなす。
    (!module || text.contains("global")) && !collect_bindings_named(root, source, name).is_empty()
}

/// 基本の集合 (解析済み) と、その外の大域の宣言を持ち得る TS のファイルのうち名前が現れ得るもので、
/// `name` を大域に宣言しているか。外のファイルを読めない・上限を超える・解析できないときは true。
fn scan_globals(
    base: &RemovalInventory,
    outside: &OutsideScan,
    budget: &Budget,
    name: &str,
) -> bool {
    if base.parsed().any(|(path, source, root, lang)| {
        lang != LangId::Javascript && declares_global_in(path, source, root, name)
    }) {
        return true;
    }
    let needles = Needles::new(vec![Needle::word(name)]);
    for source in outside.global_sources() {
        let Some(scan) = scan_file(&source.path, &needles, budget) else {
            return true;
        };
        if !scan.found_any() && !scan.escapes_any(name) {
            continue;
        }
        let Some((text, tree)) = parse_outside(&source.path, budget) else {
            return true;
        };
        let root = tree.root_node();
        if root.has_error() || declares_global_in(&source.relative, text.as_ref(), root, name) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::super::outside::module_word;
    use super::super::removed_params::RemovalEvidence;
    use super::super::source_scan::ScanLimits;
    use super::*;

    const SOURCE: &str = "src/app.tsx";

    fn write_files(root: &Path, files: &[(&str, &str)]) {
        for (path, source) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, source).unwrap();
        }
    }

    fn evidence(files: &[(&str, &str)]) -> (tempfile::TempDir, Option<RemovalEvidence>) {
        let directory = tempfile::tempdir().unwrap();
        write_files(directory.path(), files);
        write_files(directory.path(), &[(SOURCE, "export const app = 1;\n")]);
        let evidence = RemovalEvidence::load(directory.path().to_str().unwrap(), &[]);
        (directory, evidence)
    }

    /// 在庫を作れる構成で、監査を返す。
    fn audit(files: &[(&str, &str)]) -> (tempfile::TempDir, Option<RepositoryAudit>) {
        let (directory, evidence) = evidence(files);
        let evidence = evidence.unwrap_or_else(|| panic!("inventory: {files:?}"));
        (directory, evidence.repository)
    }

    fn standard(files: &[(&str, &str)]) -> bool {
        audit(files).1.is_some_and(|audit| audit.jsx_standard())
    }

    #[test]
    fn repository_audit_accepts_react_defaults_and_rejects_replaced_factories() {
        for files in [
            vec![],
            vec![(
                "tsconfig.json",
                r#"{ "compilerOptions": { "jsx": "react-jsx" } }"#,
            )],
            vec![(
                "tsconfig.json",
                "{ /* JSONC */ \"compilerOptions\": { \"jsxImportSource\": \"react\", }, }",
            )],
            vec![
                ("tsconfig.json", r#"{ "extends": "./tsconfig.base" }"#),
                (
                    "tsconfig.base.json",
                    r#"{ "compilerOptions": { "strict": true } }"#,
                ),
            ],
            // React と無関係な paths と、ローカルの react を持たない baseUrl は許す。
            vec![(
                "tsconfig.json",
                r#"{ "compilerOptions": { "baseUrl": ".", "paths": { "@/*": ["./src/*"], "~/*": ["./*"], "react-dom/*": ["./x/*"] } } }"#,
            )],
            // 中の型の置き場と、既定と同じ node_modules/@types。
            vec![(
                "tsconfig.json",
                r#"{ "compilerOptions": { "typeRoots": ["./node_modules/@types", "./types"], "types": ["node"] }, "include": ["src/**/*", "./types"] }"#,
            )],
        ] {
            assert!(standard(&files), "{files:?}");
        }
        for files in [
            vec![(
                "tsconfig.json",
                r#"{ "compilerOptions": { "jsxFactory": "h" } }"#,
            )],
            vec![(
                "jsconfig.json",
                r#"{ "compilerOptions": { "jsxFragmentFactory": "Fragment" } }"#,
            )],
            vec![(
                "packages/app/tsconfig.app.json",
                r#"{ "compilerOptions": { "jsxImportSource": "preact" } }"#,
            )],
            vec![
                ("tsconfig.json", r#"{ "extends": ["./configs/base.json"] }"#),
                (
                    "configs/base.json",
                    r#"{ "compilerOptions": { "jsxImportSource": "solid-js" } }"#,
                ),
            ],
            vec![
                (
                    "tsconfig.json",
                    r#"{ "files": [], "references": [{ "path": "./configs" }] }"#,
                ),
                (
                    "configs/tsconfig.json",
                    r#"{ "compilerOptions": { "jsxImportSource": "preact" } }"#,
                ),
            ],
            // registry と決められない型のパッケージを、大域に取り込む。
            vec![
                (
                    "tsconfig.json",
                    r#"{ "compilerOptions": { "types": ["custom-jsx"] } }"#,
                ),
                (
                    "package.json",
                    r#"{ "devDependencies": { "custom-jsx": "file:./vendor/custom-jsx" } }"#,
                ),
            ],
        ] {
            assert!(!standard(&files), "{files:?}");
        }
        // 読めない設定・外やパッケージを指す鎖・外や node_modules を取り込む設定は、監査そのもの
        // を作らない。
        for files in [
            vec![("tsconfig.json", r#"{ "extends": "@tsconfig/strictest" }"#)],
            vec![("tsconfig.json", r#"{ "extends": "./missing" }"#)],
            vec![("tsconfig.json", r#"{ "compilerOptions": [] }"#)],
            vec![("tsconfig.json", "{ /* unterminated")],
            vec![(
                "tsconfig.json",
                r#"{ "references": [{ "path": "./missing.json" }] }"#,
            )],
            vec![(
                "tsconfig.json",
                r#"{ "files": ["../type-env/env.d.ts", "src/app.tsx"] }"#,
            )],
            vec![(
                "tsconfig.json",
                r#"{ "include": ["src", "src/../../outside/**/*"] }"#,
            )],
            vec![(
                "tsconfig.json",
                r#"{ "include": ["src", "node_modules/jsx-types/**/*"] }"#,
            )],
            vec![(
                "tsconfig.json",
                r#"{ "compilerOptions": { "typeRoots": ["../type-roots"] } }"#,
            )],
            vec![(
                "tsconfig.json",
                r#"{ "compilerOptions": { "typeRoots": ["./node_modules/custom-types"] } }"#,
            )],
            vec![(
                "tsconfig.json",
                r#"{ "compilerOptions": { "types": ["../outside/jsx"] } }"#,
            )],
        ] {
            assert!(audit(&files).1.is_none(), "{files:?}");
        }
    }

    #[test]
    fn repository_audit_rejects_paths_and_base_url_that_replace_the_runtime() {
        for key in [
            "react",
            "react/*",
            "react/jsx-runtime",
            "react/jsx-dev-runtime",
            "*",
            "re*",
            "react*",
            "*-runtime",
            "react/*/x",
        ] {
            let config =
                format!(r#"{{ "compilerOptions": {{ "paths": {{ "{key}": ["./x"] }} }} }}"#);
            assert!(!standard(&[("tsconfig.json", &config)]), "{key}");
        }
        for path in ["vendor/react/jsx-runtime.ts", "vendor/react.d.ts"] {
            assert!(
                !standard(&[
                    (
                        "tsconfig.json",
                        r#"{ "compilerOptions": { "baseUrl": "./vendor" } }"#,
                    ),
                    (path, "export {};\n"),
                ]),
                "{path}"
            );
        }
        // node_modules (registry の名前を通らずに React の型を指し得る) や外を指す置き換え先と
        // baseUrl。
        for options in [
            r#""paths": { "r": ["./node_modules/@types/react"] }"#,
            r#""paths": { "@shared/*": ["../shared/*"] }"#,
            r#""paths": { "x": ["/abs/x"] }"#,
            r#""baseUrl": "./src", "paths": { "@lib/*": ["../../lib/*"] }"#,
            r#""baseUrl": "../""#,
            r#""baseUrl": "./node_modules""#,
            // `\` は TypeScript が区切りとして読むが、同じに解けない。
            r#""baseUrl": "..\\shared""#,
            // `*` の後ろの `..` も、置き換えた先を外へ出す (空の捕獲は 1 段浅い)。
            r#""paths": { "@env/*": ["./src/*/../../../shared/react-env"] }"#,
            r#""paths": { "@env/*": ["./src/*/../../shared/react-env"] }"#,
        ] {
            let config = format!(r#"{{ "compilerOptions": {{ {options} }} }}"#);
            assert!(!standard(&[("tsconfig.json", &config)]), "{options}");
        }
    }

    #[test]
    fn reaches_outside_counts_specifiers_that_leave_the_directory() {
        let root = Path::new("/repo");
        let parse = |path: &str, source: &str, lang: LangId| {
            let tree = crate::engine::parser::parse_source(source.as_bytes(), lang).unwrap();
            reaches_outside(
                root,
                path,
                tree.root_node(),
                source.as_bytes(),
                lang == LangId::Javascript,
                // 別名の無い構成では、`..` を含む指定子だけが外へ出得る。
                &|specifier: &str| specifier.contains(".."),
            )
        };
        let reaches = |path: &str, source: &str| parse(path, source, LangId::Typescript);
        // JS では、JSDoc の `@import` と `import("…")` 型も型の依存になる。
        for source in [
            "/** @import { X } from \"../../shared/react-env\" */\nexport {};\n",
            "/** @type {import(\"../../shared/react-env\").X} */\nvar x;\nexport {};\n",
        ] {
            assert!(parse("src/env.js", source, LangId::Javascript), "{source}");
            // TS のコメントは型の依存にならない。
            assert!(!parse("src/env.ts", source, LangId::Typescript), "{source}");
        }
        // 説明文の引用符と、複数行の JSDoc の行頭の `*` に左右されない。
        for source in [
            "/** Don't discard this dependency.\n * @import { X } from \"../../shared/react-env\"\n */\nexport {};\n",
            "/** @type {import(\n * \"../../shared/react-env\"\n * ).X} */\nlet x;\nexport {};\n",
            "/**\n * @import { X }\n * from\n * \"../../shared/react-env\"\n */\nexport {};\n",
            // `from` という名前を import しても、タグの中の後ろの `from` を読む。
            "/** @import { from } from \"../../shared/tag-adapter\" */\nexport {};\n",
            "/** @import { from as streamFrom } from \"../../shared/tag-adapter\" */\nexport {};\n",
        ] {
            assert!(parse("src/env.js", source, LangId::Javascript), "{source}");
        }
        // 出典の `from "URL"` のような、JSDoc の依存でない文字列は数えない。
        for source in [
            "// don't touch '../lib/x' or \"./y\"\nexport {};\n",
            "/** See \"https://example.com/docs\" for usage. */\nexport {};\n",
            "/** Adapted from \"https://react.dev/learn\". */\nexport {};\n",
            "/** @import { X } from \"../lib/x\" */\nexport {};\n",
        ] {
            assert!(!parse("src/env.js", source, LangId::Javascript), "{source}");
        }
        let long = format!("import \"{}../../shared/react-env\";\n", "./".repeat(2050));
        for (path, source) in [
            ("src/env.ts", "import '../../shared/react-env';\n"),
            // 別名の指定子は `bare` が決める (ここでは `..` を含むものだけ)。
            ("src/env.ts", "import '@env/x../shared/react-env';\n"),
            ("src/a/b.ts", "export * from \"../../../outside\";\n"),
            ("index.ts", "const x = require(`../x`);\n"),
            ("src/env.ts", "import '../..';\n"),
            ("src/env.ts", "import x from '@alias/../../outside';\n"),
            ("src/env.ts", "import x from '/abs/x';\n"),
            ("src/env.ts", "import x from 'C:/abs/x';\n"),
            // escape と行の継続は解かない。
            (
                "src/env.ts",
                "import x from '\\x2e\\x2e/\\x2e\\x2e/shared/react-env';\n",
            ),
            (
                "src/env.ts",
                "import x from \".\\\n./../shared/react-env\";\n",
            ),
            ("src/env.ts", long.as_str()),
            ("src/env.ts", "declare module '../../outside/x' {}\n"),
            ("src/env.ts", "type T = typeof import('../../outside');\n"),
            ("src/env.ts", "import x = require('../../outside');\n"),
        ] {
            assert!(reaches(path, source), "{path}: {source}");
        }
        for (path, source) in [
            ("src/env.ts", "import '../lib/x';\nimport './y';\n"),
            ("src/a/b.ts", "import '../../index';\n"),
            // 指定子でない文字列とコメント、TypeScript が解決しない式の指定子は数えない。
            (
                "src/env.ts",
                "const p = path.resolve(__dirname, '../../outside');\n",
            ),
            (
                "src/env.ts",
                "export const label = \"ok\"; // ../../README.md\n",
            ),
            ("src/env.ts", "const m = import(`../../${name}`);\n"),
            ("src/env.ts", "const m = require(name);\n"),
            (
                "src/env.ts",
                "import React from 'react';\nimport x from '@/lib/x';\n",
            ),
            (
                "src/env.ts",
                "import { PrismaClient } from '.prisma/client';\n",
            ),
        ] {
            assert!(!reaches(path, source), "{path}: {source}");
        }
    }

    #[test]
    fn repository_audit_rejects_sources_that_import_outside_the_directory() {
        assert!(!standard(&[(
            "src/env.ts",
            "import \"../../shared/react-env\";\n"
        )]));
        // 無視したファイル (在庫の外) も同じく数える。
        assert!(!standard(&[
            (".ignore", "generated/\n"),
            ("generated/env.ts", "import \"../../shared/react-env\";\n"),
        ]));
        // 対照: 中を指す相対の import は許す。
        assert!(standard(&[
            ("src/env.ts", "import \"../lib/x\";\n"),
            ("lib/x.ts", "export {};\n"),
        ]));
    }

    #[test]
    fn repository_audit_rejects_type_packages_with_manifests() {
        let config = r#"{ "compilerOptions": { "types": ["helpers"], "typeRoots": ["./types"] } }"#;
        for (manifest, entry) in [
            (
                "types/helpers/package.json",
                r#"{ "types": "../../../shared/react-types.ts" }"#,
            ),
            (
                "types/@scope/helpers/package.json",
                r#"{ "types": "../../../../shared/react-types.ts" }"#,
            ),
            (
                "types/helpers/package.json",
                r#"{ "exports": { ".": { "types": "../../../shared/react-types.ts" } } }"#,
            ),
            (
                "types/helpers/package.json",
                r#"{ "typesVersions": { "*": { "*": ["../../../shared/*"] } } }"#,
            ),
        ] {
            assert!(
                !standard(&[("tsconfig.json", config), (manifest, entry)]),
                "{manifest}: {entry}"
            );
        }
        // `types` に書いた名前は、typeRoots の下のどの深さも型の入口にできる。
        assert!(!standard(&[
            (
                "tsconfig.json",
                r#"{ "compilerOptions": { "types": ["helpers/nested"], "typeRoots": ["./types"] } }"#,
            ),
            (
                "types/helpers/nested/package.json",
                r#"{ "types": "../../../../shared/react-env.ts" }"#,
            ),
        ]));
        // typeRoots の外でも、ディレクトリの import・相対の `types`・reference から package.json の
        // 入口を使える。
        let outside_entry = r#"{ "types": "../../../shared/react-env.ts" }"#;
        for files in [
            vec![("src/env.ts", "import \"../types/nested\";\n")],
            vec![(
                "tsconfig.json",
                r#"{ "compilerOptions": { "types": ["./types/nested"] } }"#,
            )],
            vec![(
                "src/env.ts",
                "/// <reference types=\"../types/nested\" />\nexport {};\n",
            )],
        ] {
            let mut files = files;
            files.push(("types/nested/package.json", outside_entry));
            assert!(!standard(&files), "{files:?}");
        }
        // `*` を含む区切りの固定の `.` は、捕獲の `.` と合わせて `..` になり得る。
        assert!(!standard(&[(
            "types/pkg/package.json",
            r#"{ "types": "x.", "typesVersions": { "*": { "x*": ["./src/.*/../../../shared/react-env"] } } }"#,
        )]));
        // 対照: 宣言ファイルだけの型のパッケージと、中を指す入口の package.json は、中身を監査して
        // 許す。
        assert!(standard(&[
            ("tsconfig.json", config),
            ("types/helpers/index.d.ts", "export {};\n"),
        ]));
        assert!(standard(&[
            ("tsconfig.json", config),
            (
                "types/helpers/package.json",
                r#"{ "types": "./index.d.ts", "exports": { ".": { "types": "./index.d.ts" }, "./*": "./lib/*.js" } }"#,
            ),
            ("types/helpers/index.d.ts", "export {};\n"),
        ]));
    }

    #[test]
    fn repository_audit_rejects_ancestor_manifests_that_reach_outside() {
        // 解析するディレクトリの祖先の package.json の `imports` は `#` の指定子を、`exports` は名前
        // での自己参照を、監査しない外へ解き得る。
        for (ancestor, expected) in [
            (
                r##"{ "imports": { "#env": "./shared/react-env.d.ts" } }"##,
                false,
            ),
            (
                r#"{ "name": "parent", "exports": "./shared/index.ts" }"#,
                false,
            ),
            (r#"{ "name": "parent", "private": true }"#, true),
        ] {
            let directory = tempfile::tempdir().unwrap();
            write_files(
                directory.path(),
                &[
                    ("package.json", ancestor),
                    ("repo/src/app.tsx", "export const app = 1;\n"),
                ],
            );
            let evidence =
                RemovalEvidence::load(directory.path().join("repo").to_str().unwrap(), &[])
                    .unwrap_or_else(|| panic!("inventory: {ancestor}"));
            assert_eq!(
                evidence.repository.unwrap().jsx_standard(),
                expected,
                "{ancestor}"
            );
        }
    }

    #[test]
    fn repository_audit_resolves_config_dir_and_implicit_allow_js() {
        // `${configDir}` は、使う設定のディレクトリに置き換えてから解く。
        for config in [
            r#"{ "compilerOptions": { "paths": { "@env": ["${configDir}/../shared/react-env"] } } }"#,
            r#"{ "files": ["src/app.tsx", "${configDir}/../shared/react-env.ts"] }"#,
            r#"{ "compilerOptions": { "baseUrl": "${configDir}/.." } }"#,
        ] {
            assert!(!standard(&[("tsconfig.json", config)]), "{config}");
        }
        assert!(standard(&[(
            "tsconfig.json",
            r#"{ "include": ["${configDir}/src"], "compilerOptions": { "paths": { "@/*": ["${configDir}/src/*"] } } }"#,
        )]));
        // jsconfig は allowJs を既定で有効にするので、JS の import も外を指せばプログラムに入る。
        let js = (
            "src/env.js",
            "import \"../../shared/react-env\";\nexport {};\n",
        );
        assert!(!standard(&[
            ("jsconfig.json", r#"{ "compilerOptions": {} }"#),
            js
        ]));
        assert!(!standard(&[
            (
                "tsconfig.json",
                r#"{ "compilerOptions": { "allowJs": true, "checkJs": true } }"#,
            ),
            (
                "src/env.js",
                "/** @import { X } from \"../../shared/react-env\" */\nexport {};\n",
            ),
        ]));
        // 対照: JSDoc の依存でない JS のコメントの文字列は数えない。
        assert!(standard(&[
            (
                "tsconfig.json",
                r#"{ "compilerOptions": { "allowJs": true, "checkJs": true } }"#,
            ),
            (
                "src/docs.js",
                "/** See \"https://example.com/docs\" for usage. */\nexport {};\n",
            ),
        ]));
        // `${configDir}` の baseUrl も、ローカルの react を探す。
        assert!(!standard(&[
            (
                "tsconfig.json",
                r#"{ "compilerOptions": { "baseUrl": "${configDir}/.local" } }"#,
            ),
            (
                ".local/react/jsx-runtime.d.ts",
                "export * from \"custom-runtime\";\n"
            ),
        ]));
        // 対照: `${configDir}` の include は、書いた設定とそれを引き継ぐ設定でだけ解く。
        assert!(standard(&[
            (
                "tsconfig.base.json",
                r#"{ "compilerOptions": { "strict": true } }"#
            ),
            (
                "tsconfig.json",
                r#"{ "extends": "./tsconfig.base.json", "include": ["src"] }"#,
            ),
            (
                "tools/tsconfig.json",
                r#"{ "extends": "../tsconfig.base.json", "include": ["${configDir}/../src"] }"#,
            ),
        ]));
        // 対照: 引き継ぐ設定が自分の include を書けば、基底の `${configDir}` の include は効かない。
        assert!(standard(&[
            (
                "config/tsconfig.base.json",
                r#"{ "compilerOptions": { "strict": true }, "include": ["${configDir}/../src"] }"#,
            ),
            (
                "tsconfig.json",
                r#"{ "extends": "./config/tsconfig.base.json", "include": ["src"] }"#,
            ),
        ]));
        // 対照: 複数の extends では、後の設定の include が前の設定の include を上書きする。
        assert!(standard(&[
            (
                "config/tsconfig.base.json",
                r#"{ "compilerOptions": { "strict": true }, "include": ["${configDir}/../src"] }"#,
            ),
            ("tsconfig.app.json", r#"{ "include": ["src"] }"#),
            (
                "tsconfig.json",
                r#"{ "extends": ["./config/tsconfig.base.json", "./tsconfig.app.json"] }"#,
            ),
        ]));
        // 上書きしなければ、基底の include は引き継ぐ設定のディレクトリで解く。
        assert!(!standard(&[
            (
                "config/tsconfig.base.json",
                r#"{ "compilerOptions": { "strict": true }, "include": ["${configDir}/../src"] }"#,
            ),
            (
                "tsconfig.json",
                r#"{ "extends": "./config/tsconfig.base.json" }"#
            ),
        ]));
        // 対照: allowJs の無い tsconfig では、JS はプログラムに入らない。
        assert!(standard(&[
            ("tsconfig.json", r#"{ "compilerOptions": {} }"#),
            js
        ]));
        assert!(standard(&[
            (
                "jsconfig.json",
                r#"{ "compilerOptions": { "allowJs": false } }"#
            ),
            js
        ]));
    }

    #[test]
    fn repository_audit_resolves_aliases_with_the_actual_capture() {
        let config = r#"{ "compilerOptions": { "paths": { "@env/x*": ["./src/.*suffix/../../shared/react-env"], "@/*": ["./src/*"] } } }"#;
        // `/` をまたぐ捕獲 (`./a`) は、置き換え先の固定の `.` と合わせて `..` になる。
        assert!(!standard(&[
            ("tsconfig.json", config),
            ("src/env.ts", "import \"@env/x./a\";\n"),
        ]));
        // 対照: 中に収まる捕獲と、`.` で始まる区切りに置き換わる普通の別名、パッケージ名。
        for source in [
            "import \"@env/xa\";\n",
            "import type { Schema } from \"@/.generated/schema\";\n",
            "import { PrismaClient } from \".prisma/client\";\n",
        ] {
            assert!(
                standard(&[("tsconfig.json", config), ("src/env.ts", source)]),
                "{source}"
            );
        }
    }

    #[test]
    fn repository_audit_resolves_paths_against_related_base_urls() {
        // extends でつながる設定の baseUrl は、paths の解決に効く。
        let child = r#"{ "extends": "../../tsconfig.base.json", "compilerOptions": { "paths": { "@x/*": ["../../*"] } } }"#;
        assert!(!standard(&[
            (
                "tsconfig.base.json",
                r#"{ "compilerOptions": { "baseUrl": "./packages" } }"#,
            ),
            ("apps/web/tsconfig.json", child),
        ]));
        // 対照: 共通の基底の設定を持つ兄弟の設定は、それぞれの実効の baseUrl で解く。
        assert!(standard(&[
            (
                "tsconfig.base.json",
                r#"{ "compilerOptions": { "strict": true } }"#
            ),
            (
                "tsconfig.json",
                r#"{ "extends": "./tsconfig.base.json", "compilerOptions": { "baseUrl": ".", "paths": { "@app/*": ["src/*"] } } }"#,
            ),
            (
                "tools/tsconfig.json",
                r#"{ "extends": "../tsconfig.base.json", "compilerOptions": { "baseUrl": ".", "paths": { "@tool/*": ["../src/*"] } } }"#,
            ),
        ]));
        // 対照: つながらない設定の baseUrl とは組み合わせない。
        assert!(standard(&[
            (
                "tsconfig.json",
                r#"{ "compilerOptions": { "baseUrl": ".", "paths": { "@app/*": ["src/*"] } } }"#,
            ),
            (
                "tools/tsconfig.json",
                r#"{ "compilerOptions": { "baseUrl": ".", "paths": { "@app/*": ["../src/*"] } } }"#,
            ),
        ]));
    }

    #[test]
    fn repository_audit_reads_every_config_regardless_of_ignore() {
        // `.ignore` は git が無くても既定の走査から外れる。どの設定も ignore に関係なく読む。
        for (files, expected) in [
            (
                vec![
                    (".ignore", "tsconfig.json\n"),
                    (
                        "tsconfig.json",
                        r#"{ "compilerOptions": { "jsxImportSource": "preact" } }"#,
                    ),
                ],
                false,
            ),
            (
                vec![
                    (".ignore", "tsconfig.json\n"),
                    (
                        "tsconfig.json",
                        r#"{ "compilerOptions": { "jsx": "react-jsx" } }"#,
                    ),
                ],
                true,
            ),
            (
                vec![
                    (".ignore", "tsconfig.app.json\n"),
                    ("tsconfig.json", r#"{ "compilerOptions": {} }"#),
                    (
                        "tsconfig.app.json",
                        r#"{ "compilerOptions": { "jsxImportSource": "preact" } }"#,
                    ),
                ],
                false,
            ),
            (
                vec![
                    (".ignore", "tools/\n"),
                    ("tsconfig.json", r#"{ "compilerOptions": {} }"#),
                    (
                        "tools/tsconfig.json",
                        r#"{ "compilerOptions": { "jsxFactory": "h" } }"#,
                    ),
                ],
                false,
            ),
            // node_modules の中の設定は、registry の依存として信頼する。
            (
                vec![(
                    "node_modules/lib/tsconfig.json",
                    r#"{ "compilerOptions": { "jsxFactory": "h" } }"#,
                )],
                true,
            ),
        ] {
            assert_eq!(standard(&files), expected, "{files:?}");
        }
    }

    #[test]
    fn repository_audit_rejects_jsx_declarations_in_any_source() {
        for (path, source) in [
            (
                "types/env.d.ts",
                "declare namespace JSX { interface IntrinsicAttributes {} }\n",
            ),
            (
                "src/runtime.ts",
                "namespace Types { export interface Element {} }\nexport { Types as JSX };\n",
            ),
            ("src/alias.ts", "import JSX = Other.Types;\n"),
            // React の `JSX` の名前空間でないものを `JSX` に束縛する import。
            ("src/default.ts", "import JSX from \"react\";\nexport {};\n"),
            (
                "src/renamed.ts",
                "import { Fragment as JSX } from \"react\";\nexport {};\n",
            ),
            (
                "src/preact.ts",
                "import { JSX } from \"preact\";\nexport {};\n",
            ),
            (
                "src/pragma.tsx",
                "/** @jsxImportSource preact */\nexport const v = 1;\n",
            ),
            // TypeScript は pragma の名前の大文字小文字を区別しない。
            (
                "src/upper.tsx",
                "/** @JSXImportSource preact */\nexport const v = 1;\n",
            ),
            (
                "types/react.d.ts",
                "import 'react';\ndeclare module 'react' { interface Attributes { legacy?: never } }\n",
            ),
            (
                "types/runtime.d.ts",
                "declare module 'react/jsx-runtime' { export const jsx: unknown; }\n",
            ),
            (
                "src/missing.ts",
                "/// <reference path=\"./missing.d.ts\" />\nexport {};\n",
            ),
            (
                "src/nolib.ts",
                "/// <reference no-default-lib=\"true\" />\nexport {};\n",
            ),
            (
                "src/malformed.ts",
                "/// <reference path=../x.d.ts />\nexport {};\n",
            ),
            ("src/escaped.ts", "export const \\u0061 = 1;\n"),
            // `.d.ts` は `declare` を省いた module 宣言も書ける。
            (
                "types/bare.d.ts",
                "import 'react';\nmodule 'react' { function memo(value: unknown): never; }\n",
            ),
            // `@types/react` と、node_modules を通るパスも、`react` と同じ宣言ファイルを指す。
            (
                "src/types.ts",
                "import '@types/react';\ndeclare module '@types/react' { interface Attributes { legacy?: never } }\n",
            ),
            (
                "src/env.ts",
                "import '../node_modules/@types/react';\ndeclare module '../node_modules/@types/react' { function memo(value: unknown): never; }\n",
            ),
        ] {
            let (directory, audit) = audit(&[
                (".ignore", "types/\n"),
                (path, source),
                ("node_modules/types/jsx.d.ts", "export {};\n"),
            ]);
            assert!(
                !audit.unwrap().jsx_standard(),
                "{path}: {source} in {directory:?}"
            );
        }
        // 対照: 型の参照・リポジトリの中を指す reference・無関係な augmentation は許す。
        for (path, source) in [
            (
                "src/view.tsx",
                "export function v(): JSX.Element { return <b />; }\n",
            ),
            (
                "src/refs.ts",
                "/// <reference path=\"./env.d.ts\" />\nexport {};\n",
            ),
            (
                "src/refs.ts",
                "/// <reference path = './env' />\nexport {};\n",
            ),
            (
                "src/env.d.ts",
                "declare module '*.svg' { const url: string; export default url; }\n",
            ),
            (
                "src/types.ts",
                "/// <reference types=\"node\" />\n/// <reference lib=\"dom\" />\nexport {};\n",
            ),
            // リポジトリの中のモジュールの augmentation (`module.exports` の語も含む)。
            (
                "src/augment.ts",
                "import './store';\ndeclare module './store' { interface Store { extra?: string } }\n",
            ),
            ("src/legacy.js", "module.exports = { value: 1 };\n"),
            // React 19 の型で `JSX.Element` を書くための import は、React の型をそのまま参照する。
            (
                "src/other.tsx",
                "import type { JSX } from \"react\";\nexport function Other(): JSX.Element { return <b />; }\n",
            ),
            (
                "src/other.tsx",
                "import { type JSX, useState } from \"react\";\nexport const s = useState;\nexport type E = JSX.Element;\n",
            ),
        ] {
            let (_directory, audit) = audit(&[
                (path, source),
                (
                    "src/env.d.ts",
                    "declare module '*.svg' { const url: string; export default url; }\n",
                ),
            ]);
            assert!(audit.unwrap().jsx_standard(), "{path}: {source}");
        }
    }

    #[test]
    fn repository_audit_reads_reference_attributes_like_typescript() {
        let outside = "../../type-env/env.d.ts";
        // 外か node_modules を取り込む reference は、空白・タブ・どちらの引用符・大文字でも
        // 読み、監査を作らない。
        for directive in [
            format!("/// <reference path=\"{outside}\" />"),
            format!("/// <reference path = \"{outside}\" />"),
            format!("/// <reference path\t=\t'{outside}' />"),
            format!("///<reference\tpath='{outside}'/>"),
            format!("/// <REFERENCE PATH=\"{outside}\" />"),
            format!("/// <reference resolution-mode=\"import\" path=\"{outside}\" />"),
            "/// <reference path=\"../node_modules/jsx-types/index.d.ts\" />".to_owned(),
            "/// <reference types=\"../../type-env\" />".to_owned(),
        ] {
            let source = format!("{directive}\nexport {{}};\n");
            let (_directory, audit) = audit(&[("src/refs.ts", &source)]);
            assert!(audit.is_none(), "{directive}");
        }
        // 対照: 中を指す reference は許す。
        for directive in [
            "/// <reference path = \"./env.d.ts\" />",
            "/// <reference path\t=\t'./env.d.ts' />",
            "/// <REFERENCE PATH=\"./env\" />",
        ] {
            let source = format!("{directive}\nexport {{}};\n");
            let (_directory, audit) =
                audit(&[("src/refs.ts", &source), ("src/env.d.ts", "export {};\n")]);
            assert!(audit.unwrap().jsx_standard(), "{directive}");
        }
        // directive でない (4 本の `/`・ブロックコメント・タグの後に空白が無い) ものは読まない。
        for comment in [
            format!("//// <reference path=\"{outside}\" />"),
            format!("/* <reference path=\"{outside}\" /> */"),
            format!("/// <reference/> path=\"{outside}\""),
        ] {
            let source = format!("{comment}\nexport {{}};\n");
            let (_directory, audit) = audit(&[("src/refs.ts", &source)]);
            assert!(audit.unwrap().jsx_standard(), "{comment}");
        }
        assert_eq!(
            reference_attribute(" pathx=\"a\" path = 'b' ", "path"),
            Some("b")
        );
        assert_eq!(reference_attribute(" path=b ", "path"), None);
    }

    #[test]
    fn repository_audit_finds_globals_declared_in_the_repository() {
        let (_directory, loaded) = evidence(&[
            (".ignore", "types/\n"),
            (
                "types/globals.d.ts",
                "interface PanelOptions { value: string }\n",
            ),
            (
                "src/env.ts",
                "export {};\ndeclare global { type GlobalTheme = 'light'; }\n",
            ),
            ("src/local.ts", "export interface ModuleOnly { a: 1 }\n"),
        ]);
        let evidence = loaded.unwrap();
        assert!(evidence.repository.is_some());
        let declares = |name: &str| evidence.declares_global(name);
        // ignore した types/globals.d.ts は基本の集合の外で、名前が現れるので解析する。
        assert!(declares("PanelOptions"));
        assert!(declares("GlobalTheme"));
        assert!(!declares("ModuleOnly"));
        assert!(!declares("Record"));
    }

    #[test]
    fn repository_audit_distinguishes_registry_dependencies() {
        let registry = |files: &[(&str, &str)], specifier: &str| {
            let (_directory, loaded) = evidence(files);
            loaded
                .unwrap()
                .relaxation()
                .unwrap()
                .is_registry_dependency(specifier)
        };
        let manifest = (
            "package.json",
            r#"{ "name": "app", "dependencies": { "react": "^18.2.0", "alias": "npm:other@^1" }, "devDependencies": { "local": "file:./vendor/local", "linked": "link:../linked", "shared": "workspace:*", "gitdep": "github:user/repo", "pathdep": "../pathdep", "tarball": "https://example.com/x.tgz", "catalogued": "catalog:" } }"#,
        );
        for specifier in [
            "react",
            "react/jsx-runtime",
            "alias",
            "lodash",
            "node:fs",
            "@scope/pkg/sub",
        ] {
            assert!(registry(&[manifest], specifier), "{specifier}");
        }
        for specifier in [
            "local",
            "linked",
            "shared",
            "gitdep",
            "pathdep",
            "tarball",
            "catalogued",
            "app",
            "./panel",
            "#internal",
            "~/x",
            "https://example.com/x",
            "",
        ] {
            assert!(!registry(&[manifest], specifier), "{specifier}");
        }
        // ワークスペースのパッケージ・パスの別名・baseUrl の下のローカルのモジュール。
        assert!(!registry(
            &[
                manifest,
                ("packages/bridge/package.json", r#"{ "name": "bridge" }"#)
            ],
            "bridge"
        ));
        assert!(!registry(
            &[(
                "tsconfig.json",
                r#"{ "compilerOptions": { "paths": { "bridge": ["./src/bridge.ts"] } } }"#,
            )],
            "bridge"
        ));
        assert!(!registry(
            &[
                (
                    "tsconfig.json",
                    r#"{ "compilerOptions": { "baseUrl": "./src" } }"#,
                ),
                ("src/bridge/index.ts", "export {};\n"),
            ],
            "bridge"
        ));
        // 最も近い package.json に無くても、ほかの package.json (祖先や兄弟) の registry でない
        // 指定は、巻き上げで解決され得る。
        for sibling in [
            (
                "package.json",
                r#"{ "dependencies": { "bridge": "file:../bridge" } }"#,
            ),
            (
                "packages/other/package.json",
                r#"{ "dependencies": { "bridge": "link:../../../bridge" } }"#,
            ),
            (
                "package.json",
                r#"{ "dependencies": { "bridge": "^1.0.0" }, "overrides": { "bridge": "file:../bridge" } }"#,
            ),
            (
                "package.json",
                r#"{ "resolutions": { "**/parent/bridge": "portal:../bridge" } }"#,
            ),
            (
                "package.json",
                r#"{ "pnpm": { "overrides": { "parent@1>bridge@2": "link:../bridge" } } }"#,
            ),
            (
                "package.json",
                r#"{ "pnpm": { "patchedDependencies": { "bridge@1.0.0": "patches/bridge.patch" } } }"#,
            ),
            // npm の overrides の `$名前` は、参照先の指定で決める。参照先が無ければ決められない。
            (
                "package.json",
                r#"{ "dependencies": { "donor": "file:../donor", "bridge": "^1.0.0" }, "overrides": { "bridge": "$donor" } }"#,
            ),
            (
                "package.json",
                r#"{ "dependencies": { "bridge": "^1.0.0" }, "overrides": { "bridge": "$missing" } }"#,
            ),
        ] {
            assert!(
                !registry(
                    &[sibling, ("src/app/package.json", r#"{ "name": "child" }"#)],
                    "bridge"
                ),
                "{sibling:?}"
            );
        }
        // 対照: registry の指定を参照する `$名前` は、registry のまま。
        assert!(registry(
            &[(
                "package.json",
                r#"{ "dependencies": { "react": "^18.2.0" }, "overrides": { "react": "$react" } }"#,
            )],
            "react"
        ));
        // 読めない package.json は、監査そのものを作らない。
        for broken in [
            "{ broken",
            r#"{ "dependencies": { "x": 1 } }"#,
            r#"{ "overrides": { "*": "1.0.0" } }"#,
        ] {
            let (_directory, audit) = audit(&[("package.json", broken)]);
            assert!(audit.is_none(), "{broken}");
        }
    }

    #[test]
    fn repository_packages_resolve_bare_specifiers_to_the_target() {
        let (_directory, loaded) = evidence(&[
            (
                "tsconfig.json",
                r#"{ "compilerOptions": { "paths": { "@ui": ["./src/panel.tsx"], "@/*": ["./src/*"], "@missing/*": ["./nowhere/*"] } } }"#,
            ),
            ("src/panel.tsx", "export const p = 1;\n"),
            ("src/other.ts", "export const o = 1;\n"),
            ("packages/kit/package.json", r#"{ "name": "kit" }"#),
            ("packages/kit/index.ts", "export {};\n"),
            (
                "package.json",
                r#"{ "name": "app", "dependencies": { "react": "^18.2.0", "local": "file:./vendor/local" } }"#,
            ),
        ]);
        let evidence = loaded.unwrap();
        let packages = evidence.relaxation().unwrap();
        let reach = |specifier: &str| packages.bare_may_reach(specifier, "src/panel.tsx");
        // paths の別名は置き換え先で決める。置き換え先が無ければ決められない。
        assert!(reach("@ui"));
        assert!(reach("@/panel"));
        assert!(!reach("@/other"));
        assert!(reach("@missing/x"));
        // リポジトリの package は、宣言のファイルを含むときだけ。自己参照は全体を含む。
        assert!(!reach("kit"));
        assert!(reach("app"));
        // registry の依存は届かない。registry でない依存と、パッケージを指さない指定子は
        // 決められない。
        assert!(!reach("react"));
        assert!(reach("local"));
        assert!(reach("#internal"));
    }

    #[test]
    fn repository_packages_read_pnpm_and_yarn_configs() {
        let registry = |files: &[(&str, &str)], specifier: &str| -> Option<bool> {
            let (_directory, loaded) = evidence(files);
            loaded
                .unwrap()
                .relaxation()
                .map(|packages| packages.is_registry_dependency(specifier))
        };
        let manifest = (
            "package.json",
            r#"{ "name": "app", "dependencies": { "bridge": "^1.0.0", "react": "catalog:", "ui": "catalog:shared" } }"#,
        );
        // pnpm-workspace.yaml の overrides・packageExtensions・パッチ・catalog の、registry でない指定。
        for yaml in [
            "overrides:\n  bridge: link:../bridge\n",
            "overrides:\n  parent>bridge: file:./vendor/bridge\n",
            "packageExtensions:\n  parent@1:\n    dependencies:\n      bridge: link:../bridge\n",
            "patchedDependencies:\n  bridge@1.0.0: patches/bridge.patch\n",
            "catalog:\n  bridge: link:../bridge\n",
            "catalogs:\n  shared:\n    bridge: github:user/bridge\n",
        ] {
            assert_eq!(
                registry(&[manifest, ("pnpm-workspace.yaml", yaml)], "bridge"),
                Some(false),
                "{yaml}"
            );
        }
        // .yarnrc.yml の packageExtensions も同じ規則で決める。
        assert_eq!(
            registry(
                &[
                    manifest,
                    (
                        ".yarnrc.yml",
                        "packageExtensions:\n  parent@*:\n    dependencies:\n      bridge: portal:../bridge\n",
                    ),
                ],
                "bridge"
            ),
            Some(false)
        );
        // 対照: registry の版だけの指定と、catalog の registry の指定への参照は、registry のまま。
        let yaml = "packages:\n  - packages/*\noverrides:\n  bridge: ^1.2.0\ncatalog:\n  react: ^18.2.0\ncatalogs:\n  shared:\n    ui: 2.0.0\n";
        for specifier in ["bridge", "react", "ui"] {
            assert_eq!(
                registry(&[manifest, ("pnpm-workspace.yaml", yaml)], specifier),
                Some(true),
                "{specifier}"
            );
        }
        // catalog に無い `catalog:` の参照は決められない。
        assert_eq!(registry(&[manifest], "react"), Some(false));
        // 読めない YAML・pnpmfile の場所の指定・pnpmfile・解決先を変える設定があれば、依存の判定の
        // 材料も監査も作らない。
        for files in [
            vec![
                manifest,
                ("pnpm-workspace.yaml", "overrides: [unterminated"),
            ],
            vec![manifest, ("pnpm-workspace.yaml", "pnpmfile: ./hooks.cjs\n")],
            vec![
                manifest,
                (".pnpmfile.cjs", "module.exports = { hooks: {} };\n"),
            ],
            vec![manifest, ("tools/pnpmfile.js", "module.exports = {};\n")],
            vec![manifest, (".npmrc", "pnpmfile=scripts/install-hooks.cjs\n")],
            vec![
                manifest,
                ("packages/app/.npmrc", "global-pnpmfile = ~/hooks.cjs\n"),
            ],
            vec![
                manifest,
                (
                    "tsconfig.json",
                    r#"{ "compilerOptions": { "moduleSuffixes": [".native", ""] } }"#,
                ),
            ],
            vec![
                manifest,
                (
                    "tsconfig.json",
                    r#"{ "compilerOptions": { "rootDirs": ["src", "generated"] } }"#,
                ),
            ],
        ] {
            assert_eq!(registry(&files, "bridge"), None, "{files:?}");
        }
        // 対照: hook の無い .npmrc と、解決先を変えない設定。
        for files in [
            vec![
                manifest,
                (".npmrc", "auto-install-peers=true\n# pnpmfile=x\n"),
            ],
            vec![
                manifest,
                (
                    "tsconfig.json",
                    r#"{ "compilerOptions": { "moduleSuffixes": [""], "rootDirs": ["src"] } }"#,
                ),
            ],
        ] {
            assert_eq!(registry(&files, "bridge"), Some(true), "{files:?}");
        }
    }

    #[test]
    fn repository_audit_requires_registry_react_for_jsx() {
        for manifest in [
            r#"{ "dependencies": { "react": "^18.2.0", "lodash": "^4" }, "devDependencies": { "@types/react": "^18.2.0" } }"#,
            r#"{ "dependencies": { "react": "^18.2.0" }, "overrides": { "react": "18.3.1" } }"#,
            r#"{ "dependencies": { "local": "file:./vendor/local" } }"#,
        ] {
            assert!(
                standard(&[
                    ("package.json", manifest),
                    ("vendor/local/index.js", "module.exports = {};\n"),
                ]),
                "{manifest}"
            );
        }
        for manifest in [
            r#"{ "dependencies": { "react": "file:../custom-react" } }"#,
            r#"{ "dependencies": { "react": "file:./vendor/react" } }"#,
            r#"{ "dependencies": { "react": "npm:@preact/compat" } }"#,
            r#"{ "devDependencies": { "@types/react": "npm:types-react@beta" } }"#,
            r#"{ "devDependencies": { "@types/react": "link:./vendor/types" } }"#,
            r#"{ "devDependencies": { "@types/jsx-env": "file:./vendor/types" } }"#,
            r#"{ "dependencies": { "react": "^18.2.0" }, "overrides": { "react": "github:user/react" } }"#,
            r#"{ "dependencies": { "react": "^18.2.0" }, "pnpm": { "patchedDependencies": { "@types/react@18.2.0": "patches/types.patch" } } }"#,
            // 外の中身の依存は、推移的な依存の型が大域に入り得る。
            r#"{ "dependencies": { "ui": "github:user/ui" } }"#,
        ] {
            assert!(
                !standard(&[
                    ("package.json", manifest),
                    ("vendor/react/index.js", "module.exports = {};\n"),
                    ("vendor/types/index.d.ts", "export {};\n"),
                ]),
                "{manifest}"
            );
        }
    }

    #[test]
    fn repository_audit_does_not_follow_extends_outside_the_analyzed_directory() {
        let outer = tempfile::tempdir().unwrap();
        std::fs::write(
            outer.path().join("base.json"),
            r#"{ "compilerOptions": {} }"#,
        )
        .unwrap();
        let inner = outer.path().join("repo");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(
            inner.join("tsconfig.json"),
            r#"{ "extends": "../base.json" }"#,
        )
        .unwrap();
        let evidence = RemovalEvidence::load(inner.to_str().unwrap(), &[]).unwrap();
        assert!(evidence.repository.is_none());
    }

    #[test]
    fn repository_audit_reads_ancestor_project_configs_and_workspaces() {
        let outer = tempfile::tempdir().unwrap();
        let inner = outer.path().join("packages").join("app");
        std::fs::create_dir_all(&inner).unwrap();
        let load = || {
            RemovalEvidence::load(inner.to_str().unwrap(), &[])
                .unwrap()
                .repository
        };
        assert!(load().unwrap().jsx_standard());
        // 祖先の設定が解析するディレクトリだけを取り込むなら、その JSX の設定を読む。
        std::fs::write(
            outer.path().join("tsconfig.json"),
            r#"{ "compilerOptions": { "jsxImportSource": "preact" }, "include": ["packages/app"] }"#,
        )
        .unwrap();
        assert!(!load().unwrap().jsx_standard());
        // 祖先の設定が外のファイルも取り込む (既定の include) なら、監査を作らない。
        std::fs::write(
            outer.path().join("tsconfig.json"),
            r#"{ "compilerOptions": {} }"#,
        )
        .unwrap();
        assert!(load().is_none());
        // 直下に自分の設定があれば、祖先の設定は配下のファイルに効かない。
        std::fs::write(inner.join("tsconfig.json"), r#"{ "compilerOptions": {} }"#).unwrap();
        assert!(load().unwrap().jsx_standard());
        // 外にワークスペースの定義があると、外のパッケージの名前を確かめられない。
        std::fs::write(
            outer.path().join("package.json"),
            r#"{ "workspaces": ["packages/*"] }"#,
        )
        .unwrap();
        assert!(load().is_none());
    }

    #[test]
    fn removal_evidence_splits_the_base_set_and_outside_sources() {
        let (_directory, loaded) = evidence(&[
            (".ignore", "generated/\ndist/\ntools/\nnode_modules/\n"),
            ("generated/a.tsx", "export const a = 1;\n"),
            ("dist/b.js", "export const b = 1;\n"),
            ("dist/c.vue", "<template><div /></template>\n"),
            (".storybook/d.ts", "export const d = 1;\n"),
            // ほかの言語のファイルは、基本の集合の外なら見ない。
            (".husky/pre-commit", "#!/bin/sh\nnpm test\n"),
            ("tools/script.py", "print(1)\n"),
            ("node_modules/lib/index.js", "export const e = 1;\n"),
            (".git/hooks/f.ts", "export const f = 1;\n"),
            ("README.md", "# x\n"),
        ]);
        let split = loaded.unwrap();
        let base: Vec<&str> = split.base.parsed().map(|(path, ..)| path).collect();
        assert_eq!(base, [SOURCE]);
        let mut outside: Vec<(&str, bool)> = split
            .outside_sources()
            .map(|source| (source.relative.as_str(), source.lang.is_some()))
            .collect();
        outside.sort_unstable();
        assert_eq!(
            outside,
            [
                (".storybook/d.ts", true),
                ("dist/b.js", true),
                ("dist/c.vue", false),
                ("generated/a.tsx", true)
            ]
        );
        assert!(split.repository.unwrap().jsx_standard());
        // 基本の集合の、ほかの言語のソースと解析できないソースは、この PR の前と同じく在庫を
        // 作らない。
        for (path, source) in [
            ("scripts/build.sh", "#!/bin/sh\nnpm test\n"),
            ("src/view.vue", "<template><div /></template>\n"),
            ("tools.py", "print(1)\n"),
        ] {
            let (_directory, loaded) = evidence(&[(path, source)]);
            assert!(loaded.is_none(), "{path}");
        }
    }

    #[test]
    fn removal_evidence_adds_outside_files_that_mention_the_name() {
        let directory = tempfile::tempdir().unwrap();
        let blob = "x".repeat(10_000);
        let namespace =
            "import * as m from '../src/app';\nexport const v = m[['Pa', 'nel'].join('')];\n";
        write_files(
            directory.path(),
            &[
                (SOURCE, "export function Panel() { return null; }\n"),
                (".ignore", "generated/\n"),
                (
                    "generated/uses.tsx",
                    "import { Panel } from '../src/app';\nexport const v = <Panel />;\n",
                ),
                ("generated/escaped.ts", "export const P\\u0061nel = 1;\n"),
                // 宣言のファイルの名前 (`app`) は、宣言でない TS 系のファイルでだけ語にする。
                ("generated/namespace.ts", namespace),
                ("generated/namespace.js", namespace),
                (
                    "generated/types.d.ts",
                    "import type * as m from '../src/app';\nexport type T = typeof m;\n",
                ),
                ("generated/plain.ts", "export const other = 1;\n"),
                ("generated/blob.js", &blob),
                ("generated/view.vue", "<template><div /></template>\n"),
            ],
        );
        let root = directory.path().to_str().unwrap();
        let expected = [
            "generated/escaped.ts",
            "generated/namespace.ts",
            "generated/types.d.ts",
            "generated/uses.tsx",
        ];
        // 検出の前に渡した語でも、後から照合する語でも、同じファイルを選ぶ。
        for words in [vec!["Panel".to_owned(), "app".to_owned()], Vec::new()] {
            let evidence = RemovalEvidence::load(root, &words).unwrap();
            let keys = |name: &str, target: &str| {
                evidence
                    .outside_uses(name, target)
                    .unwrap()
                    .into_keys()
                    .collect::<Vec<_>>()
            };
            // 名前・宣言のファイルを指す名前 (宣言でない TS 系だけ)・宣言のファイルへの依存
            // (`.d.ts` を含む TS 系) で選ぶ。JS は名前だけで選ぶ。
            assert_eq!(keys("Panel", SOURCE), expected, "{words:?}");
            // 依存は、実際の宣言のファイルに解決するときだけ数える。
            assert_eq!(
                keys("Panel", "src/Panel.tsx"),
                ["generated/escaped.ts", "generated/uses.tsx"]
            );
            // 名前も依存も無いファイルは、ignore されていても在庫に入れない。
            assert!(keys("Other", "src/missing.ts").is_empty());
        }
        // 読む量か解析する量の合計の上限を超えると、在庫も監査も作らない。
        for limits in [
            ScanLimits {
                stream_bytes: 4_096,
                ..ScanLimits::DEFAULT
            },
            ScanLimits {
                parse_bytes: 10,
                ..ScanLimits::DEFAULT
            },
        ] {
            let small = RemovalEvidence::load_with(root, &[], limits).unwrap();
            assert!(small.repository.is_none(), "{limits:?}");
            assert!(small.outside_uses("Panel", SOURCE).is_none(), "{limits:?}");
        }
        // 在庫に加えるファイルは、パスごとに 1 度だけ解析し、共有の解析の予算に数える。走査で
        // 解析したファイル (印か依存のあるもの) と同じ量を、もう一度だけ使える。
        let parsed: u64 = expected
            .iter()
            .map(|path| {
                std::fs::metadata(directory.path().join(path))
                    .unwrap()
                    .len()
            })
            .sum();
        let limited = |parse_bytes: u64| {
            RemovalEvidence::load_with(
                root,
                &[],
                ScanLimits {
                    parse_bytes,
                    ..ScanLimits::DEFAULT
                },
            )
            .unwrap()
        };
        let enough = limited(2 * parsed);
        assert!(enough.outside_uses("Panel", SOURCE).is_some());
        assert!(enough.outside_uses("Panel", SOURCE).is_some());
        assert!(
            limited(2 * parsed - 1)
                .outside_uses("Panel", SOURCE)
                .is_none()
        );
        // 埋め込みのソースに名前が現れると証明しない。
        write_files(
            directory.path(),
            &[("generated/view.vue", "<template><Panel /></template>\n")],
        );
        let evidence = RemovalEvidence::load(root, &[]).unwrap();
        assert!(evidence.outside_uses("Panel", SOURCE).is_none());
        assert!(evidence.outside_uses("Other", "src/missing.ts").is_some());
        assert_eq!(module_word("src/panel.tsx"), Some("panel"));
    }

    #[cfg(unix)]
    #[test]
    fn repository_audit_rejects_symlinks_that_hide_files() {
        let link = |target: &str, link: &str| {
            let directory = tempfile::tempdir().unwrap();
            write_files(
                directory.path(),
                &[
                    (SOURCE, "export const app = 1;\n"),
                    ("real.txt", "{}"),
                    ("docs/guide.md", "# guide\n"),
                ],
            );
            let outside = tempfile::tempdir().unwrap();
            write_files(
                outside.path(),
                &[(
                    "env.d.ts",
                    "declare namespace JSX { interface IntrinsicAttributes {} }\n",
                )],
            );
            let target = target.replace("{outside}", outside.path().to_str().unwrap());
            let path = directory.path().join(link);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(directory.path().join(target), &path).unwrap();
            let evidence = RemovalEvidence::load(directory.path().to_str().unwrap(), &[]).unwrap();
            (directory, outside, evidence)
        };
        for (target, name) in [
            ("real.txt", "tsconfig.json"),
            ("real.txt", "package.json"),
            ("real.txt", "src/link.ts"),
            ("{outside}", "types"),
            ("src", "mirror"),
            ("missing.ts", "src/broken.ts"),
        ] {
            let (_directory, _outside, evidence) = link(target, name);
            assert!(evidence.repository.is_none(), "{name} -> {target}");
        }
        // 対照: 在庫・監査の対象にならないファイルへの symlink は許す。
        let (_directory, _outside, evidence) = link("docs/guide.md", "README.md");
        assert!(evidence.repository.unwrap().jsx_standard());
    }

    #[test]
    fn repository_audit_package_names_and_path_keys() {
        for (specifier, expected) in [
            ("react", Some("react")),
            ("react/jsx-runtime", Some("react")),
            ("@scope/pkg", Some("@scope/pkg")),
            ("@scope/pkg/sub", Some("@scope/pkg")),
            ("node:fs/promises", Some("node:fs")),
            ("@scope", None),
            ("@/config", None),
            ("./x", None),
            ("/abs", None),
            ("#internal", None),
            ("~/x", None),
            ("https://example.com/x", None),
            ("", None),
        ] {
            assert_eq!(package_name(specifier), expected, "{specifier}");
        }
        for (key, specifier, expected) in [
            ("bridge", "bridge", true),
            ("bridge", "bridge/sub", false),
            ("bridge/*", "bridge/sub", true),
            ("*", "anything", true),
            ("@/*", "react", false),
            ("a*b*c", "x", true),
        ] {
            assert_eq!(
                path_key_matches(key, specifier),
                expected,
                "{key} {specifier}"
            );
        }
        let names = |key: &str| override_names(key);
        let list = |items: &[&str]| Some(items.iter().map(|item| (*item).to_owned()).collect());
        for (key, expected) in [
            ("react", list(&["react"])),
            ("react@18", list(&["react"])),
            ("@types/react", list(&["@types/react"])),
            ("@types/react@^18", list(&["@types/react"])),
            ("parent>react", list(&["parent", "react"])),
            ("parent@1>@types/react@2", list(&["parent", "@types/react"])),
            ("**/react", list(&["react"])),
            ("parent/@types/react", list(&["parent", "@types/react"])),
            ("@scope/parent/react", list(&["@scope/parent", "react"])),
            // 範囲の中の `>`・`>=`・`<` は区切りではない。
            ("bridge@>=1.0.0", list(&["bridge"])),
            ("bridge@>1", list(&["bridge"])),
            ("bridge@1 || >2", list(&["bridge"])),
            ("@scope/bridge@>=1 <2", list(&["@scope/bridge"])),
            ("parent@1>bridge@>=2", list(&["parent", "bridge"])),
            ("parent@>1>bridge", list(&["parent", "bridge"])),
            // 解釈できない・曖昧なキー。
            ("a>b>c", None),
            ("**", None),
            ("", None),
            ("@scope", None),
            ("bad name", None),
        ] {
            assert_eq!(names(key), expected, "{key}");
        }
    }
}
