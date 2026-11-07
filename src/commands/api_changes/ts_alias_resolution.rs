//! 削除の参照の帰属 (api.rm の降格) で、相対でない import の指定子を tsconfig / jsconfig の
//! `paths`・`baseUrl` とパッケージの依存で解く (Issue #264)。
//!
//! 指定子 1 つを、参照に適用され得る全ての設定 (`dir` の中の `tsconfig*.json` / `jsconfig*.json`、
//! その `references` の先、`dir` の直下に無ければ祖先のいちばん近い tsconfig.json / jsconfig.json)
//! で解いて合わせる。1 つの設定では TypeScript と同じく、`paths` のキーに当たれば完全一致か最長の
//! 接頭辞のパターンの置換先を、当たらなければ `baseUrl` の下を探し、どちらでも解けなければ native の
//! 解決 (node_modules・`typeRoots`・package.json の `imports`、classic なら参照ファイルの祖先の
//! ディレクトリ) へ進む。TypeScript の版で既定の解決の方式が変わる (TS 5 と TS 6 で、`module` の
//! 指定から決まる方式が違う) ので、あり得る方式のどれでも解決先が候補に入るように集める。
//!
//! どれかの設定で解決先を網羅できなければ [`AliasResolution::Unknown`] にし、ほかの設定の結果で
//! 消さない。網羅できないのは、読めない・解けない設定 (辿れない `extends`、`moduleSuffixes` など)、
//! node16 / nodenext の設定でローカルに当たる指定子 (ESM では拡張子を補わない)、classic になり得る
//! 設定でファイルとしては解けない置換先、package.json のあるディレクトリ、symlink を通るパス、
//! native の解決でリポジトリの中へ届き得る指定子 (`#` の subpath import・リポジトリのパッケージの
//! 名前・registry でない依存・依存に無いパッケージ・`typeRoots` の下にあるもの)、置換先や classic の
//! 祖先の探索に削除したファイルがあって native の解決へ進む指定子 (削除前は削除したファイルに
//! 解かれていた)。
//!
//! native の解決で外部のパッケージとみなすのは、参照ファイルのディレクトリから祖先へ解析する
//! ディレクトリのルートまでの package.json のどれかに registry の指定で書いた依存 (Issue #280。
//! ワークスペースのパッケージにだけ書いた依存も、そのパッケージのファイルからは外部)。祖先でない
//! package.json にだけ書いた依存は、hoisting で解決されても宣言からは決められないので決めない。
//! 実際の配置 (node_modules・PnP) は確かめず、宣言から分類する。
//!
//! 前提 (信頼の境界): 実行環境の解決先 (バンドラーの alias、Jest の moduleNameMapper、実行時の
//! loader、コマンドラインで渡す設定、pnpmfile や pnpm-workspace.yaml などのパッケージマネージャの
//! 設定) は、ここで解いた tsconfig / jsconfig と package.json の解決先と一致する。どの設定も別名を
//! 解かない指定子は、バンドラーだけが解く別名かもしれないので証明しない。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use serde_json::{Map, Value};

use super::ts_module_paths::{
    RESOLVED_EXTENSIONS, may_be_same_path, may_be_under, normalize_lexically, package_name,
    registry_spec, with_suffix,
};
use crate::engine::bounded_read::read_utf8_file_limited;
use crate::git_support::normalize_workspace_separators;

mod strip_internal;
pub(crate) use strip_internal::StripInternalConfigs;

const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_CONFIG_TOTAL_BYTES: usize = 8 * 1024 * 1024;
const MAX_MANIFEST_TOTAL_BYTES: usize = 32 * 1024 * 1024;
const MAX_CHAIN_DEPTH: usize = 8;
const MAX_ANCESTORS: usize = 128;
const PROJECT_CONFIG_NAMES: [&str; 2] = ["tsconfig.json", "jsconfig.json"];
const DEPENDENCY_FIELDS: [&str; 4] = [
    "dependencies",
    "devDependencies",
    "peerDependencies",
    "optionalDependencies",
];
/// 解決に効く `compilerOptions` のキー。
const RESOLUTION_OPTIONS: [&str; 7] = [
    "paths",
    "baseUrl",
    "module",
    "moduleResolution",
    "target",
    "moduleSuffixes",
    "typeRoots",
];
/// TS の探索 (宣言ファイルを含む) が、拡張子の無い置換先に補う拡張子。
const TS_PASS_EXTENSIONS: [&str; 3] = [".ts", ".tsx", ".d.ts"];
const DECLARATION_EXTENSIONS: [&str; 3] = [".d.ts", ".d.mts", ".d.cts"];
const CONFIG_DIR: &str = "${configDir}";

/// 指定子 1 つの解決の結果。
#[derive(Clone, Debug)]
pub(crate) enum AliasResolution {
    /// リポジトリの中のファイルに解かれる。候補は `dir` 相対 (`/` 区切り) で、存在しないものも含み、
    /// 実際の解決先はこの中にある。
    Local(Rc<HashSet<String>>),
    /// 参照ファイルの祖先 (解析するディレクトリのルートまで) の package.json に registry の指定で
    /// 書いた依存のパッケージ。リポジトリのファイルではない。
    External,
    /// 解決先を決められない。
    Unknown,
}

/// 解析するディレクトリの設定と package.json から作った、相対でない指定子の解決器。検出 1 回に
/// 閉じる。
pub(crate) struct TsAliasResolver {
    root: PathBuf,
    /// 削除したシンボルのファイル (`dir` 相対)。native の解決へ進む前に試した候補にあれば、
    /// 削除前はそのファイルに解かれていたので決めない。
    removed_files: HashSet<String>,
    /// 設定ごとの実効値。内の None は読めない・解けない設定、外の None は走査できなかった。
    projects: Option<Vec<Option<Effective>>>,
    /// package.json から読んだ事実。None は読めない package.json があった。
    packages: Option<Packages>,
    /// (参照ファイルのディレクトリ, 指定子) → 結果。
    memo: HashMap<(String, String), AliasResolution>,
    /// 読んだ設定のどれも、相対の指定子の探索を変えない (`moduleSuffixes`・`rootDirs` が無い)。
    /// 設定を走査できなければ false。
    relative_unchanged: bool,
}

impl TsAliasResolver {
    pub(crate) fn load(dir: &str, removed_files: HashSet<String>) -> Self {
        let root = std::path::absolute(Path::new(dir))
            .ok()
            .and_then(|path| path.canonicalize().ok());
        let mut resolver = Self {
            root: root.clone().unwrap_or_default(),
            removed_files,
            projects: None,
            packages: None,
            memo: HashMap::new(),
            relative_unchanged: false,
        };
        let Some(root) = root else {
            return resolver;
        };
        let Some(walked) = walk(&root) else {
            return resolver;
        };
        resolver.packages = read_packages(&root, &walked.manifests);
        if let Some((projects, relative_unchanged)) = load_projects(&root, &walked.configs) {
            resolver.projects = Some(projects);
            resolver.relative_unchanged = relative_unchanged;
        }
        resolver
    }

    /// 相対の指定子を [`relative_import_candidates`](super::removed_attribution::relative_import_candidates)
    /// で網羅できるか。どの設定も相対の探索を変えない (`moduleSuffixes`・`rootDirs` が無い) とき
    /// だけ true。
    pub(crate) fn relative_resolution_unchanged(&self) -> bool {
        self.relative_unchanged
    }

    /// `ref_path` (`dir` 相対) のファイルにある相対でない指定子 `spec` を解く。
    pub(crate) fn resolve(&mut self, ref_path: &str, spec: &str) -> AliasResolution {
        // native の解決で数える package.json (祖先のもの) と classic の祖先の探索が、参照ファイルの
        // 位置に依る。
        let from = Path::new(ref_path)
            .parent()
            .and_then(Path::to_str)
            .map(normalize_workspace_separators)
            .unwrap_or_default();
        let key = (from, spec.to_owned());
        if let Some(resolution) = self.memo.get(&key) {
            return resolution.clone();
        }
        let resolution = self.resolve_uncached(&key.0, spec);
        self.memo.insert(key, resolution.clone());
        resolution
    }

    fn resolve_uncached(&self, from: &str, spec: &str) -> AliasResolution {
        // 絶対パス・相対の形の指定子と `\` を含む指定子は、paths の外でファイルを直接指し得る。
        // 途中に `.` / `..` の区切りがある指定子 (`pkg/../../src/x`) は、パッケージの外へ出得る。
        if spec.is_empty()
            || spec.starts_with(['/', '.'])
            || spec.contains('\\')
            || Path::new(spec).is_absolute()
            || spec.split('/').any(|segment| matches!(segment, "." | ".."))
        {
            return AliasResolution::Unknown;
        }
        let Some(projects) = &self.projects else {
            return AliasResolution::Unknown;
        };
        let mut reach = Reach {
            native: projects.is_empty(),
            ..Reach::default()
        };
        for project in projects {
            let Some(project_reach) = project
                .as_ref()
                .and_then(|effective| self.project_reach(effective, spec))
            else {
                return AliasResolution::Unknown;
            };
            reach.absorb(project_reach);
        }
        if reach.classic_native {
            // classic は参照ファイルのディレクトリから祖先へ、指定子の名前のファイルを探す。
            let Some(probe) = self.classic_ancestors(from, spec) else {
                return AliasResolution::Unknown;
            };
            if probe.any_hit {
                reach.locals.extend(probe.candidates);
            } else if self.removes_any(&probe.candidates) {
                return AliasResolution::Unknown;
            }
        }
        let native = reach.native.then(|| self.native(from, spec));
        if native == Some(Native::Unknown) {
            return AliasResolution::Unknown;
        }
        if !reach.locals.is_empty() {
            return AliasResolution::Local(Rc::new(reach.locals));
        }
        match native {
            Some(Native::External) => AliasResolution::External,
            _ => AliasResolution::Unknown,
        }
    }

    /// 候補に削除したファイルがあるか (大文字小文字を区別しないファイルシステムも考える)。
    fn removes_any(&self, candidates: &HashSet<String>) -> bool {
        candidates.iter().any(|candidate| {
            self.removed_files
                .iter()
                .any(|removed| may_be_same_path(removed, candidate))
        })
    }

    /// 設定 1 つで `spec` を解く。TypeScript の版で解釈が分かれる置換 (空の捕獲) は解釈ごとに解き、
    /// どれかが native の解決へ進み得るなら、それも残す。決められなければ None。
    fn project_reach(&self, effective: &Effective, spec: &str) -> Option<Reach> {
        let interpretations = match effective
            .paths
            .as_ref()
            .map(|paths| paths.interpretations(spec))
        {
            Some(Some(Some(interpretations))) => interpretations,
            Some(None) => return None,
            // paths に当たらなければ baseUrl の下を探す (paths に当たったら baseUrl は見ない)。
            Some(Some(None)) | None => match &effective.base_url {
                Some(base) => vec![vec![Substitution {
                    path: base.join(spec),
                    directory_only: spec.ends_with('/'),
                }]],
                None => {
                    return Some(Reach {
                        native: true,
                        classic_native: effective.mode == Mode::MaybeClassic,
                        ..Reach::default()
                    });
                }
            },
        };
        let mut reach = Reach::default();
        for substitutions in interpretations {
            let mut total = Probe::default();
            for substitution in &substitutions {
                total.absorb(self.probe(substitution)?);
            }
            let (local, native) = match effective.mode {
                // ESM では拡張子の無い置換先を補わず、CJS では補う。参照ファイルの形式で解決先が
                // 変わる。
                Mode::Node16 if total.any_hit => return None,
                // classic は置換先をファイルとしてしか解かない (index・package.json を経ない)。
                Mode::MaybeClassic if total.file_ts_hit => (true, false),
                Mode::MaybeClassic if total.any_hit => return None,
                Mode::Standard if total.ts_hit => (true, false),
                // JS のファイルにしか当たらなければ、node10 の TS の探索は先に native の解決を見る。
                Mode::Standard if total.any_hit => (true, true),
                Mode::Node16 | Mode::MaybeClassic | Mode::Standard => (false, true),
            };
            // native の解決へ進む前に試した候補に削除したファイルがあれば、削除前はそこへ
            // 解かれていた。
            if !local && self.removes_any(&total.candidates) {
                return None;
            }
            if local {
                reach.locals.extend(total.candidates);
            }
            reach.native |= native;
            reach.classic_native |= native && effective.mode == Mode::MaybeClassic;
        }
        Some(reach)
    }

    /// 置換先 1 つをモジュールとして解く候補と、当たったか。
    fn probe(&self, substitution: &Substitution) -> Option<Probe> {
        let joined = normalize_lexically(&substitution.path)?;
        let inner = joined.strip_prefix(&self.root).ok()?;
        if inner.as_os_str().is_empty() {
            return None;
        }
        passes_without_links(&self.root, inner)?;
        let mut probe = Probe::default();
        let ts_files = if substitution.directory_only {
            Vec::new()
        } else {
            ts_pass_files(&joined)
        };
        for candidate in module_file_candidates(&joined) {
            if !substitution.directory_only && is_regular_file(&candidate)? {
                probe.any_hit = true;
                if ts_files.contains(&candidate) {
                    probe.ts_hit = true;
                    probe.file_ts_hit = true;
                }
            }
            probe.candidates.insert(self.relative(&candidate)?);
        }
        let indexes: Vec<PathBuf> = RESOLVED_EXTENSIONS
            .iter()
            .map(|extension| joined.join(format!("index{extension}")))
            .collect();
        if !probe.file_ts_hit && is_directory(&joined)? {
            // package.json は types・typings・main・exports でディレクトリの中のどれでも指し得る。
            if is_regular_file(&joined.join("package.json"))? {
                return None;
            }
            for index in &indexes {
                if is_regular_file(index)? {
                    probe.any_hit = true;
                    if TS_PASS_EXTENSIONS
                        .iter()
                        .any(|extension| index.ends_with(format!("index{extension}")))
                    {
                        probe.ts_hit = true;
                    }
                }
            }
        }
        for index in indexes {
            probe.candidates.insert(self.relative(&index)?);
        }
        Some(probe)
    }

    /// classic の祖先の探索: `from` (`dir` 相対) から `dir` まで、各ディレクトリの下の `spec` を
    /// ファイルとして解く候補。`dir` の外は削除したファイルではないので見ない。
    fn classic_ancestors(&self, from: &str, spec: &str) -> Option<Probe> {
        let mut probe = Probe::default();
        let mut directory = self.root.join(from);
        loop {
            if let Some(joined) = normalize_lexically(&directory.join(spec))
                && let Ok(inner) = joined.strip_prefix(&self.root)
                && !inner.as_os_str().is_empty()
            {
                passes_without_links(&self.root, inner)?;
                for candidate in module_file_candidates(&joined) {
                    probe.any_hit |= is_regular_file(&candidate)?;
                    probe.candidates.insert(self.relative(&candidate)?);
                }
            }
            if directory == self.root || !directory.pop() || !directory.starts_with(&self.root) {
                break;
            }
        }
        Some(probe)
    }

    fn relative(&self, path: &Path) -> Option<String> {
        let inner = path.strip_prefix(&self.root).ok()?.to_str()?;
        Some(normalize_workspace_separators(inner))
    }

    /// paths と baseUrl で解けないときの native の解決が、リポジトリの中へ届き得るか。`from` は
    /// 参照ファイルのディレクトリ (`dir` 相対)。
    fn native(&self, from: &str, spec: &str) -> Native {
        if spec.starts_with('#') {
            // package.json の `imports` は、パッケージの中のどのファイルも指し得る。
            return Native::Unknown;
        }
        let Some(name) = package_name(spec) else {
            return Native::NotPackage;
        };
        let (Some(packages), Some(projects)) = (&self.packages, &self.projects) else {
            return Native::Unknown;
        };
        if packages.local_overrides
            || packages.names.contains(name)
            || packages.non_registry.contains(name)
        {
            // ワークスペースや自己参照、`file:` などの依存は、node_modules からリポジトリの中を指す。
            return Native::Unknown;
        }
        // node10 は node_modules で型を見つけられないと、設定した `typeRoots` の下のパッケージも
        // 探す (`@scope/name` は `scope__name` でも)。node_modules の下は registry の型なので見ない。
        let mangled = name
            .strip_prefix('@')
            .map(|scoped| scoped.replacen('/', "__", 1));
        for type_root in projects
            .iter()
            .flatten()
            .flat_map(|project| &project.type_roots)
            .filter(|type_root| !has_node_modules(type_root))
        {
            // symlink の先も、正規化した位置で削除したファイルと比べる。
            let Some(real) = (match is_directory(type_root) {
                Some(true) => type_root.canonicalize().ok(),
                Some(false) => Some(type_root.clone()),
                None => match type_root.canonicalize() {
                    Ok(real) if is_directory(&real) == Some(true) => Some(real),
                    _ => None,
                },
            }) else {
                return Native::Unknown;
            };
            for root in [type_root, &real] {
                for entry in std::iter::once(name).chain(mangled.as_deref()) {
                    let package = root.join(entry);
                    let declaration = with_suffix(&package, ".d.ts");
                    let (Some(directory), Some(file)) =
                        (is_directory(&package), is_regular_file(&declaration))
                    else {
                        return Native::Unknown;
                    };
                    if directory || file {
                        return Native::Unknown;
                    }
                    // 削除したファイルがそのパッケージの下か宣言ファイルなら、削除前はそこへ
                    // 解かれていた。
                    if let (Some(package), Some(declaration)) =
                        (self.relative(&package), self.relative(&declaration))
                        && self.removed_files.iter().any(|removed| {
                            may_be_same_path(removed, &declaration)
                                || may_be_under(removed, &package)
                        })
                    {
                        return Native::Unknown;
                    }
                }
            }
        }
        if packages.declares_registry(from, name) {
            Native::External
        } else {
            // 祖先の package.json の依存に無いパッケージは、どこから入るか決められない (ほかの
            // ワークスペースのパッケージの依存が hoisting で見えるのは、配置に依る)。
            Native::Unknown
        }
    }
}

/// 設定 (と解釈) ごとに解いた結果を合わせたもの。
#[derive(Default)]
struct Reach {
    /// 実際の解決先になり得るローカルの候補 (`dir` 相対)。
    locals: HashSet<String>,
    /// native の解決へ進み得る。
    native: bool,
    /// classic になり得る設定で native の解決へ進み得る (祖先のディレクトリも探す)。
    classic_native: bool,
}

impl Reach {
    fn absorb(&mut self, other: Reach) {
        self.locals.extend(other.locals);
        self.native |= other.native;
        self.classic_native |= other.classic_native;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Native {
    /// 参照ファイルの祖先 (解析するディレクトリまで) の package.json の registry の依存。
    External,
    /// パッケージ名として成り立たない (`@/x`・`~/x`)。node_modules からはリポジトリのファイルに
    /// 解かれない。
    NotPackage,
    Unknown,
}

#[derive(Default)]
struct Probe {
    candidates: HashSet<String>,
    /// TS の探索 (宣言ファイルを含む) で解ける。
    ts_hit: bool,
    /// そのうち、ディレクトリを経ずにファイルとして解ける。
    file_ts_hit: bool,
    /// どれかの候補がある (JS のファイルを含む)。
    any_hit: bool,
}

impl Probe {
    fn absorb(&mut self, other: Probe) {
        self.candidates.extend(other.candidates);
        self.ts_hit |= other.ts_hit;
        self.file_ts_hit |= other.file_ts_hit;
        self.any_hit |= other.any_hit;
    }
}

/// `joined` をファイルとして解くときの候補の上位集合 (拡張子の補完・読み替え・宣言ファイル)。
/// TypeScript は指定した名前のまま、拡張子を足した名前、拡張子を読み替えた名前
/// (`.js` → `.ts`、`.ts` → `.tsx`、`.d.ts` → `.ts`)、任意の拡張子の宣言ファイル
/// (`a.css` → `a.d.css.ts`) を試す。
fn module_file_candidates(joined: &Path) -> Vec<PathBuf> {
    let mut candidates = vec![joined.to_path_buf()];
    candidates.extend(
        RESOLVED_EXTENSIONS
            .iter()
            .map(|extension| with_suffix(joined, extension)),
    );
    let Some(name) = joined.file_name().and_then(|name| name.to_str()) else {
        return candidates;
    };
    let stems = DECLARATION_EXTENSIONS
        .iter()
        .filter_map(|extension| name.strip_suffix(extension))
        .chain(name.rsplit_once('.').map(|(stem, _)| stem))
        .filter(|stem| !stem.is_empty());
    for stem in stems {
        candidates.extend(
            RESOLVED_EXTENSIONS
                .iter()
                .map(|extension| joined.with_file_name(format!("{stem}{extension}"))),
        );
    }
    if let Some((stem, extension)) = name.rsplit_once('.')
        && !stem.is_empty()
    {
        candidates.push(joined.with_file_name(format!("{stem}.d.{extension}.ts")));
    }
    candidates
}

/// TS の探索 (宣言ファイルを含む) がファイルとして必ず試す候補。この中のファイルがあれば、
/// その置換先で解ける (下位集合。`module_file_candidates` は上位集合)。
fn ts_pass_files(joined: &Path) -> Vec<PathBuf> {
    let Some(name) = joined.file_name().and_then(|name| name.to_str()) else {
        return Vec::new();
    };
    if DECLARATION_EXTENSIONS
        .iter()
        .any(|extension| name.ends_with(extension))
    {
        return vec![joined.to_path_buf()];
    }
    let stem_with = |stem: &str, extensions: &[&str]| -> Vec<PathBuf> {
        extensions
            .iter()
            .map(|extension| joined.with_file_name(format!("{stem}{extension}")))
            .collect()
    };
    match name.rsplit_once('.') {
        Some((stem, "js" | "jsx")) => stem_with(stem, &TS_PASS_EXTENSIONS),
        Some((stem, "mjs")) => stem_with(stem, &[".mts", ".d.mts"]),
        Some((stem, "cjs")) => stem_with(stem, &[".cts", ".d.cts"]),
        Some((_, "ts" | "tsx" | "mts" | "cts")) => vec![joined.to_path_buf()],
        _ => TS_PASS_EXTENSIONS
            .iter()
            .map(|extension| with_suffix(joined, extension))
            .collect(),
    }
}

/// `root` の下の `inner` の途中で、既にある要素がどれも symlink / junction でないか。読めないとき
/// も None。symlink の先は、字句の上のパスで候補を比べられない。
fn passes_without_links(root: &Path, inner: &Path) -> Option<()> {
    let mut current = root.to_path_buf();
    for component in inner.components() {
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if is_link(&metadata) => return None,
            Ok(_) => {}
            Err(error) if missing(&error) => return Some(()),
            Err(_) => return None,
        }
    }
    Some(())
}

/// 無いとみなせるエラー (Windows でファイル名に使えない `*` などを含む名前も、無いファイル)。
fn missing(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound
            | std::io::ErrorKind::NotADirectory
            | std::io::ErrorKind::InvalidFilename
    )
}

fn is_link(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink() || is_reparse_point(metadata)
}

#[cfg(windows)]
fn is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_: &std::fs::Metadata) -> bool {
    false
}

/// symlink でない通常のファイルがあるか。symlink か、読めないときは None。
fn is_regular_file(path: &Path) -> Option<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if is_link(&metadata) => None,
        Ok(metadata) => Some(metadata.is_file()),
        Err(error) if missing(&error) => Some(false),
        Err(_) => None,
    }
}

/// symlink でないディレクトリがあるか。symlink か、読めないときは None。
fn is_directory(path: &Path) -> Option<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if is_link(&metadata) => None,
        Ok(metadata) => Some(metadata.is_dir()),
        Err(error) if missing(&error) => Some(false),
        Err(_) => None,
    }
}

/// 設定 1 つの、解決に効く実効値。
struct Effective {
    paths: Option<Paths>,
    base_url: Option<PathBuf>,
    mode: Mode,
    /// `typeRoots` (書いたときだけ。既定の node_modules/@types は registry の型)。
    type_roots: Vec<PathBuf>,
}

/// 置換先 1 つ。
struct Substitution {
    path: PathBuf,
    /// `/` で終わる (TypeScript はファイルを探さずディレクトリだけを探す)。
    directory_only: bool,
}

/// 実効の `paths`。
struct Paths {
    /// (キー, 置換先) を書いた順に。
    entries: Vec<(String, Vec<String>)>,
    /// 置換先の基点 (実効の baseUrl、無ければ paths を書いた設定のディレクトリ)。
    base: PathBuf,
    /// 使う設定 (継承の末端) のディレクトリ (`${configDir}`)。
    leaf: PathBuf,
}

impl Paths {
    /// `spec` に当たるパターンの置換先を、TypeScript の版で分かれる解釈ごとに返す。当たらなければ
    /// `Some(None)`、置換先を作れなければ None。完全一致のキーを先に、無ければ接頭辞の最も長い
    /// パターン (同じ長さなら先に書いたもの) を使う。
    fn interpretations(&self, spec: &str) -> Option<Option<Vec<Vec<Substitution>>>> {
        // (捕獲した文字列。完全一致のキーは None, 置換先)
        let mut best: Option<(Option<&str>, &[String])> = None;
        let mut best_prefix = 0;
        for (key, values) in &self.entries {
            match key.split_once('*') {
                None if key == spec => {
                    best = Some((None, values.as_slice()));
                    break;
                }
                None => {}
                Some((prefix, suffix)) => {
                    if spec.len() < prefix.len() + suffix.len()
                        || !spec.starts_with(prefix)
                        || !spec.ends_with(suffix)
                    {
                        continue;
                    }
                    if best.is_none() || prefix.len() > best_prefix {
                        let captured = &spec[prefix.len()..spec.len() - suffix.len()];
                        best = Some((Some(captured), values.as_slice()));
                        best_prefix = prefix.len();
                    }
                }
            }
        }
        let Some((captured, values)) = best else {
            return Some(None);
        };
        // 完全一致のキーの置換先の `*` は置き換えない。空の捕獲は TS 6 では置き換えないので、
        // 置き換えた解釈と置き換えない解釈の両方を解く。
        let captures = match captured {
            Some("") if values.iter().any(|value| value.contains('*')) => vec![Some(""), None],
            captured => vec![captured],
        };
        let mut interpretations = Vec::new();
        for captured in captures {
            let mut substitutions = Vec::new();
            for value in values {
                // `${configDir}` は設定の値として先に展開する (指定子から捕獲した文字列は展開しない)。
                let (base, rest) = match value.strip_prefix(CONFIG_DIR) {
                    Some(rest) => (&self.leaf, rest.trim_start_matches('/')),
                    None if value.contains(CONFIG_DIR) => return None,
                    None => (&self.base, value.as_str()),
                };
                let rest = match captured {
                    Some(captured) => rest.replacen('*', captured, 1),
                    None => rest.to_owned(),
                };
                substitutions.push(Substitution {
                    path: base.join(&rest),
                    directory_only: rest.ends_with('/'),
                });
            }
            interpretations.push(substitutions);
        }
        Some(Some(interpretations))
    }
}

/// 設定の解決の方式。TypeScript の版による既定の違いは、あり得る方式のどちらでも網羅できる
/// 側に寄せる。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// node10・bundler (TS 6 の既定を含む)。ファイル・ディレクトリを解き、JS のファイルにしか
    /// 当たらなければ native の解決も見る。
    Standard,
    /// classic になり得る (TS 5 の既定。TS 6 では bundler)。置換先をファイルとしてしか解かず、
    /// native の解決では参照ファイルの祖先のディレクトリも探す。
    MaybeClassic,
    /// node16・nodenext。参照ファイルが ESM か CJS かで拡張子の補完が変わる。
    Node16,
}

/// `extends` を辿って合わせた `compilerOptions` (キー → (値, 書いた設定のディレクトリ))。
#[derive(Clone, Default)]
struct Chain {
    options: HashMap<String, (Value, PathBuf)>,
}

impl Chain {
    fn effective(&self, leaf: &Path) -> Option<Effective> {
        let get = |key: &str| self.options.get(key);
        // `moduleSuffixes` は置換先の探索を変える (`[]` と `[""]` は既定と同じ)。
        if let Some((value, _)) = get("moduleSuffixes") {
            let suffixes = value.as_array()?;
            if !(suffixes.is_empty() || (suffixes.len() == 1 && suffixes[0].as_str() == Some(""))) {
                return None;
            }
        }
        let base_url = match get("baseUrl") {
            None => None,
            Some((value, defined_in)) => Some(option_path(value.as_str()?, defined_in, leaf)?),
        };
        let type_roots = match get("typeRoots") {
            None => Vec::new(),
            Some((value, defined_in)) => value
                .as_array()?
                .iter()
                .map(|root| normalize_lexically(&option_path(root.as_str()?, defined_in, leaf)?))
                .collect::<Option<_>>()?,
        };
        let paths = match get("paths") {
            None => None,
            Some((value, defined_in)) => {
                let mut entries = Vec::new();
                for (key, values) in value.as_object()? {
                    let values = values
                        .as_array()?
                        .iter()
                        .map(|value| value.as_str().map(str::to_owned))
                        .collect::<Option<Vec<_>>>()?;
                    // TypeScript が誤りとして報告する形 (`*` が 2 個以上、空の置換先) は解かない。
                    if key.matches('*').count() > 1
                        || values.is_empty()
                        || values
                            .iter()
                            .any(|value| value.matches('*').count() > 1 || value.contains('\\'))
                    {
                        return None;
                    }
                    entries.push((key.clone(), values));
                }
                Some(Paths {
                    entries,
                    base: base_url.clone().unwrap_or_else(|| defined_in.clone()),
                    leaf: leaf.to_path_buf(),
                })
            }
        };
        Some(Effective {
            paths,
            base_url,
            mode: self.mode()?,
            type_roots,
        })
    }

    /// 実効の解決の方式。解釈できない値は None。
    fn mode(&self) -> Option<Mode> {
        let lower = |key: &str| -> Option<Option<String>> {
            match self.options.get(key) {
                None => Some(None),
                Some((value, _)) => Some(Some(value.as_str()?.to_ascii_lowercase())),
            }
        };
        Some(match lower("moduleResolution")?.as_deref() {
            Some("node16" | "nodenext") => Mode::Node16,
            Some("classic") => Mode::MaybeClassic,
            Some("node" | "node10" | "bundler") => Mode::Standard,
            Some(_) => return None,
            // 未指定なら `module` から決まる。commonjs は TS 5 で node10・TS 6 で bundler、
            // preserve は bundler、node16 などは node16、es2015 以降は TS 5 で classic・TS 6 で
            // bundler、none・amd・umd・system は classic。`module` も無ければ、`target` が ES5 以下
            // なら commonjs と同じ、ES2015 以上なら es2015 と同じ。
            None => match lower("module")?.as_deref() {
                Some("commonjs" | "preserve") => Mode::Standard,
                Some(module) if module.starts_with("node") => Mode::Node16,
                Some(_) => Mode::MaybeClassic,
                None => match lower("target")?.as_deref() {
                    None | Some("es3" | "es5") => Mode::Standard,
                    Some(_) => Mode::MaybeClassic,
                },
            },
        })
    }
}

/// 設定の値のパス (`baseUrl`・`typeRoots`) を、書いた設定のディレクトリか `${configDir}` (使う
/// 設定のディレクトリ) から解く。
fn option_path(value: &str, defined_in: &Path, leaf: &Path) -> Option<PathBuf> {
    if value.contains('\\') {
        return None;
    }
    match value.strip_prefix(CONFIG_DIR) {
        Some(rest) => Some(leaf.join(rest.trim_start_matches('/'))),
        None if value.contains(CONFIG_DIR) => None,
        None => Some(defined_in.join(value)),
    }
}

/// `root` を、node_modules と .git の中を除き、ignore の指定に関係なく走査した結果。
struct Walked {
    configs: Vec<PathBuf>,
    manifests: Vec<PathBuf>,
}

/// 設定ファイルの名前か。大文字小文字を区別しないファイルシステムでは `TSConfig.json` も
/// `tsconfig.json` として読まれるので、大文字小文字を区別しない。
fn is_config_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    (name.starts_with("tsconfig") || name.starts_with("jsconfig")) && name.ends_with(".json")
}

/// `path` の途中に node_modules があるか。
fn has_node_modules(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str().eq_ignore_ascii_case("node_modules"))
}

/// 設定と package.json を集める。先を確かめられない symlink の設定・package.json、読めない
/// エントリ (一覧を読めないディレクトリを含む。名前を知っていれば中の設定は開け得る) があれば
/// None。
fn walk(root: &Path) -> Option<Walked> {
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .standard_filters(false)
        .follow_links(false)
        .filter_entry(|entry| {
            entry.depth() == 0
                || !matches!(entry.file_name().to_str(), Some("node_modules" | ".git"))
        });
    let mut walked = Walked {
        configs: Vec::new(),
        manifests: Vec::new(),
    };
    for entry in builder.build() {
        let entry = entry.ok()?;
        if entry.depth() == 0 {
            continue;
        }
        let Some(name) = entry.file_name().to_str() else {
            continue;
        };
        let config = is_config_name(name);
        if !config && !name.eq_ignore_ascii_case("package.json") {
            continue;
        }
        let kind = entry.file_type()?;
        if entry.path_is_symlink() || kind.is_symlink() || !kind.is_file() {
            return None;
        }
        if config {
            walked.configs.push(entry.into_path());
        } else {
            walked.manifests.push(entry.into_path());
        }
    }
    walked.configs.sort();
    walked.manifests.sort();
    Some(walked)
}

/// package.json から読んだ事実。
#[derive(Default)]
struct Packages {
    /// リポジトリの package.json の `name` (ワークスペースと自己参照で、node_modules から中へ届く)。
    names: HashSet<String>,
    /// どれかの package.json に registry でない指定 (`file:`・`workspace:` など) で書いた依存の名前。
    non_registry: HashSet<String>,
    /// package.json のディレクトリ (`dir` 相対、`/` 区切り。ルートは空) ごとの、registry の指定で
    /// 書いた依存の名前。
    registry_by_directory: HashMap<String, HashSet<String>>,
    /// `overrides`・`resolutions`・`pnpm.overrides` に registry でない指定がある (どの依存も
    /// リポジトリの中を指し得る)。
    local_overrides: bool,
}

fn read_packages(root: &Path, manifests: &[PathBuf]) -> Option<Packages> {
    let mut packages = Packages::default();
    let mut bytes = 0usize;
    for path in manifests {
        let source = read_utf8_file_limited(path, MAX_FILE_BYTES)?;
        bytes = bytes.checked_add(source.len())?;
        if bytes > MAX_MANIFEST_TOTAL_BYTES {
            return None;
        }
        let value: Value = serde_json::from_str(source.trim_start_matches('\u{feff}')).ok()?;
        let manifest = value.as_object()?;
        if let Some(name) = manifest.get("name") {
            packages.names.insert(name.as_str()?.to_owned());
        }
        // 走査はルートの下だけなので、ディレクトリはルート相対にできる。
        let directory = path
            .parent()
            .and_then(|parent| parent.strip_prefix(root).ok())
            .and_then(Path::to_str)
            .map(normalize_workspace_separators);
        for field in DEPENDENCY_FIELDS {
            let Some(dependencies) = manifest.get(field) else {
                continue;
            };
            for (name, spec) in dependencies.as_object()? {
                if !registry_spec(spec.as_str()?) {
                    packages.non_registry.insert(name.clone());
                } else if let Some(directory) = &directory {
                    packages
                        .registry_by_directory
                        .entry(directory.clone())
                        .or_default()
                        .insert(name.clone());
                }
            }
        }
        let overrides = [
            manifest.get("overrides"),
            manifest.get("resolutions"),
            manifest
                .get("pnpm")
                .and_then(|pnpm| pnpm.as_object())
                .and_then(|pnpm| pnpm.get("overrides")),
        ];
        for value in overrides.into_iter().flatten() {
            packages.local_overrides |= has_local_override(value);
        }
    }
    Some(packages)
}

impl Packages {
    /// `from` (参照ファイルのディレクトリ、`dir` 相対) から祖先へ解析するディレクトリのルートまでの
    /// package.json のどれかが、`name` を registry の指定で依存に書いているか。ディレクトリは
    /// 区切りごとに完全一致で比べる (`ui` と `ui-other`、大文字小文字の違う名前を混ぜない)。
    fn declares_registry(&self, from: &str, name: &str) -> bool {
        Path::new(from).ancestors().any(|directory| {
            directory
                .to_str()
                .map(normalize_workspace_separators)
                .and_then(|directory| self.registry_by_directory.get(&directory))
                .is_some_and(|names| names.contains(name))
        })
    }
}

/// 上書きの指定 (入れ子の object を含む) に、registry でない指定があるか。読めない形も含める。
fn has_local_override(value: &Value) -> bool {
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            Value::String(spec) => {
                if !(registry_spec(spec) || spec.starts_with('$')) {
                    return true;
                }
            }
            Value::Object(entries) => pending.extend(entries.values()),
            _ => return true,
        }
    }
    false
}

/// 設定を読み、`extends` を辿って合わせる。
#[derive(Default)]
struct ConfigLoader {
    bytes: usize,
    raw: HashMap<PathBuf, Option<Rc<Map<String, Value>>>>,
    /// (設定, 辿ってよい範囲) → 合わせた結果。同じ親を何度も書いた `extends` で、探索が指数的に
    /// 増えないように覚える。
    chains: HashMap<(PathBuf, PathBuf), Option<Chain>>,
}

/// `compilerOptions` が相対の指定子の探索を変えるか: 既定でない `moduleSuffixes` (`[]`・`[""]`・
/// `null` は既定と同じ) と、2 つ以上の `rootDirs` (1 つ以下では別のルートへ展開しない)。
fn changes_relative_resolution(config: &Map<String, Value>) -> bool {
    let Some(options) = config.get("compilerOptions") else {
        return false;
    };
    let Some(options) = options.as_object() else {
        return true;
    };
    let suffixes_changed = options
        .get("moduleSuffixes")
        .is_some_and(|value| match value {
            Value::Null => false,
            Value::Array(suffixes) => {
                !(suffixes.is_empty() || (suffixes.len() == 1 && suffixes[0].as_str() == Some("")))
            }
            _ => true,
        });
    let roots_changed = options.get("rootDirs").is_some_and(|value| match value {
        Value::Null => false,
        Value::Array(roots) => roots.len() > 1,
        _ => true,
    });
    suffixes_changed || roots_changed
}

fn load_projects(root: &Path, configs: &[PathBuf]) -> Option<(Vec<Option<Effective>>, bool)> {
    let mut loader = ConfigLoader::default();
    // (設定, extends と references で辿ってよい範囲)。
    let mut pending: Vec<(PathBuf, PathBuf)> = configs
        .iter()
        .rev()
        .map(|path| (path.clone(), root.to_path_buf()))
        .collect();
    // TypeScript はファイルからいちばん近い tsconfig.json / jsconfig.json を使う。直下に無ければ、
    // 祖先の設定が配下のファイルに効き得る。
    let has_root_config = configs.iter().any(|path| {
        path.parent() == Some(root)
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    PROJECT_CONFIG_NAMES
                        .iter()
                        .any(|project| name.eq_ignore_ascii_case(project))
                })
    });
    if !has_root_config {
        for (depth, ancestor) in root.ancestors().enumerate().skip(1) {
            if depth > MAX_ANCESTORS {
                return None;
            }
            let mut found = false;
            for name in PROJECT_CONFIG_NAMES {
                let path = ancestor.join(name);
                if is_regular_file(&path)? {
                    pending.push((path, ancestor.to_path_buf()));
                    found = true;
                }
            }
            if found {
                break;
            }
        }
    }
    let mut seen = HashSet::new();
    let mut projects = Vec::new();
    let mut relative_unchanged = true;
    while let Some((path, area)) = pending.pop() {
        let Some(canonical) = regular_file(&path) else {
            projects.push(None);
            continue;
        };
        if !seen.insert(canonical.clone()) {
            continue;
        }
        let Some(leaf) = canonical.parent().map(Path::to_path_buf) else {
            projects.push(None);
            continue;
        };
        let Some(raw) = loader.read(&canonical) else {
            projects.push(None);
            continue;
        };
        // solution 形式の設定では、参照先のプロジェクトが独立して効く (継承はしない)。
        if let Some(references) = raw.get("references") {
            let Some(references) = references.as_array() else {
                projects.push(None);
                continue;
            };
            for reference in references {
                match reference
                    .as_object()
                    .and_then(|reference| reference.get("path"))
                    .and_then(Value::as_str)
                    .and_then(|path| referenced_path(&leaf, path, &area))
                {
                    Some(referenced) => pending.push((referenced, area.clone())),
                    None => projects.push(None),
                }
            }
        }
        projects.push(
            loader
                .chain(&canonical, &area, 0, &mut Vec::new())
                .and_then(|chain| chain.effective(&leaf)),
        );
        relative_unchanged &= loader.relative_unchanged(&canonical, &area, 0, &mut HashSet::new());
    }
    Some((projects, relative_unchanged))
}

impl ConfigLoader {
    /// 上限付きで読み、JSONC として解く。読めない・解けないものは None。
    fn read(&mut self, canonical: &Path) -> Option<Rc<Map<String, Value>>> {
        if let Some(raw) = self.raw.get(canonical) {
            return raw.clone();
        }
        let raw = read_utf8_file_limited(canonical, MAX_FILE_BYTES).and_then(|source| {
            self.bytes = self.bytes.checked_add(source.len())?;
            if self.bytes > MAX_CONFIG_TOTAL_BYTES {
                return None;
            }
            let bytes = super::jsonc::json_bytes(source.trim_start_matches('\u{feff}'))?;
            match serde_json::from_slice(&bytes).ok()? {
                Value::Object(map) => Some(Rc::new(map)),
                _ => None,
            }
        });
        self.raw.insert(canonical.to_path_buf(), raw.clone());
        raw
    }

    /// `canonical` の設定と `extends` の先のどれも、相対の指定子の探索を変えないか
    /// ([`changes_relative_resolution`])。[`Self::chain`] と違い、途中の設定を解釈できなくても
    /// 辿れる先は全て見る。読めない・解けない設定は、字面に `moduleSuffixes` も `rootDirs` も
    /// 無いときだけ変えないとみなす。辿れない `extends` の先 (入っていないパッケージの設定) は、
    /// 変えないと仮定する (信頼の境界)。
    fn relative_unchanged(
        &mut self,
        canonical: &Path,
        area: &Path,
        depth: usize,
        visited: &mut HashSet<PathBuf>,
    ) -> bool {
        if depth > MAX_CHAIN_DEPTH {
            return false;
        }
        if !visited.insert(canonical.to_path_buf()) {
            return true;
        }
        let Some(raw) = self.read(canonical) else {
            return read_utf8_file_limited(canonical, MAX_FILE_BYTES).is_some_and(|text| {
                !text.contains("moduleSuffixes") && !text.contains("rootDirs")
            });
        };
        if changes_relative_resolution(&raw) {
            return false;
        }
        let Some(directory) = canonical.parent().map(Path::to_path_buf) else {
            return false;
        };
        let parents: Vec<String> = match raw.get("extends") {
            Some(Value::String(parent)) => vec![parent.clone()],
            Some(Value::Array(parents)) => parents
                .iter()
                .filter_map(|parent| parent.as_str().map(str::to_owned))
                .collect(),
            _ => Vec::new(),
        };
        parents.iter().all(|parent| {
            self.extended_path(&directory, parent, area)
                .is_none_or(|path| self.relative_unchanged(&path, area, depth + 1, visited))
        })
    }

    /// `canonical` の設定の `compilerOptions` を、`extends` の先から順に上書きして合わせる。
    fn chain(
        &mut self,
        canonical: &Path,
        area: &Path,
        depth: usize,
        visiting: &mut Vec<PathBuf>,
    ) -> Option<Chain> {
        if depth > MAX_CHAIN_DEPTH || visiting.iter().any(|path| path == canonical) {
            return None;
        }
        let key = (canonical.to_path_buf(), area.to_path_buf());
        if let Some(chain) = self.chains.get(&key) {
            return chain.clone();
        }
        visiting.push(canonical.to_path_buf());
        let chain = self.chain_uncached(canonical, area, depth, visiting);
        visiting.pop();
        self.chains.insert(key, chain.clone());
        chain
    }

    fn chain_uncached(
        &mut self,
        canonical: &Path,
        area: &Path,
        depth: usize,
        visiting: &mut Vec<PathBuf>,
    ) -> Option<Chain> {
        let raw = self.read(canonical)?;
        let directory = canonical.parent()?.to_path_buf();
        let mut chain = Chain::default();
        let parents: Vec<&str> = match raw.get("extends") {
            None => Vec::new(),
            Some(Value::String(parent)) => vec![parent.as_str()],
            Some(Value::Array(parents)) => {
                parents.iter().map(Value::as_str).collect::<Option<_>>()?
            }
            Some(_) => return None,
        };
        // 配列の extends は後ろの設定が優先し、最後に自身の設定が優先する。
        for parent in parents {
            let parent = self.extended_path(&directory, parent, area)?;
            let inherited = self.chain(&parent, area, depth + 1, visiting)?;
            chain.options.extend(inherited.options);
        }
        if let Some(options) = raw.get("compilerOptions") {
            let options = options.as_object()?;
            for key in RESOLUTION_OPTIONS {
                match options.get(key) {
                    None => {}
                    // null で継承を打ち消す書き方は、解釈を確かめていないので読まない。
                    Some(Value::Null) => return None,
                    Some(value) => {
                        chain
                            .options
                            .insert(key.to_owned(), (value.clone(), directory.clone()));
                    }
                }
            }
        }
        Some(chain)
    }

    /// `extends` の先。相対・絶対のパスは TypeScript と同じく `.json` を補い、パッケージは設定の
    /// 位置から `area` までの node_modules で探す (package.json に `exports` があれば解かない)。
    /// `area` の外は None。
    fn extended_path(&mut self, directory: &Path, parent: &str, area: &Path) -> Option<PathBuf> {
        if parent.contains('\\') || parent.contains(CONFIG_DIR) {
            return None;
        }
        let found =
            if parent.starts_with("./") || parent.starts_with("../") || parent.starts_with('/') {
                let base = normalize_lexically(&directory.join(parent))?;
                if is_regular_file(&base)? || parent.ends_with(".json") {
                    base
                } else {
                    with_suffix(&base, ".json")
                }
            } else {
                self.package_config(directory, parent, area)?
            };
        let canonical = regular_file(&found)?;
        canonical.starts_with(area).then_some(canonical)
    }

    /// パッケージの設定 (`@tsconfig/node20/tsconfig.json`・`@tsconfig/node20`) を、`directory`
    /// から `area` までの祖先の node_modules で探す。pnpm のようにパッケージのディレクトリが
    /// symlink でも、正規化した先が `area` の中にあるときだけ読む。
    fn package_config(&mut self, directory: &Path, spec: &str, area: &Path) -> Option<PathBuf> {
        let name = package_name(spec)?;
        let subpath = spec[name.len()..].trim_start_matches('/');
        for ancestor in directory.ancestors() {
            if !ancestor.starts_with(area) {
                return None;
            }
            let Ok(package) = ancestor.join("node_modules").join(name).canonicalize() else {
                continue;
            };
            if !package.starts_with(area) {
                return None;
            }
            if !is_directory(&package)? {
                continue;
            }
            let manifest_path = package.join("package.json");
            let manifest = if is_regular_file(&manifest_path)? {
                Some(self.read(&manifest_path)?)
            } else {
                None
            };
            if manifest
                .as_ref()
                .is_some_and(|manifest| manifest.contains_key("exports"))
            {
                return None;
            }
            if subpath.is_empty() {
                let field = match manifest
                    .as_ref()
                    .and_then(|manifest| manifest.get("tsconfig"))
                {
                    Some(field) => field.as_str()?.to_owned(),
                    None => "tsconfig.json".to_owned(),
                };
                return Some(package.join(field));
            }
            let base = normalize_lexically(&package.join(subpath))?;
            return Some(if is_regular_file(&base)? || subpath.ends_with(".json") {
                base
            } else {
                with_suffix(&base, ".json")
            });
        }
        None
    }
}

/// symlink でない通常のファイルなら、その正規化したパス。
fn regular_file(path: &Path) -> Option<PathBuf> {
    is_regular_file(path)?.then(|| path.canonicalize().ok())?
}

/// `references` の `path` (設定ファイルか、tsconfig.json を持つディレクトリ)。`area` の外は None。
fn referenced_path(directory: &Path, path: &str, area: &Path) -> Option<PathBuf> {
    if path.contains('\\') {
        return None;
    }
    let target = normalize_lexically(&directory.join(path))?;
    let file = if is_directory(&target)? {
        target.join("tsconfig.json")
    } else {
        target
    };
    let canonical = regular_file(&file)?;
    canonical.starts_with(area).then_some(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, files: &[(&str, &str)]) {
        for (path, content) in files {
            let full = root.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, content).unwrap();
        }
    }

    /// `src/app/use.ts` の `spec` を解き、存在する候補か結果の種類を返す。
    fn resolve_removing(files: &[(&str, &str)], removed: &[&str], spec: &str) -> String {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), files);
        let mut resolver = TsAliasResolver::load(
            dir.path().to_str().unwrap(),
            removed.iter().map(|path| (*path).to_owned()).collect(),
        );
        match resolver.resolve("src/app/use.ts", spec) {
            AliasResolution::Local(candidates) => {
                let mut existing: Vec<&String> = candidates
                    .iter()
                    .filter(|candidate| dir.path().join(candidate).is_file())
                    .collect();
                existing.sort();
                format!(
                    "local:{}",
                    existing
                        .iter()
                        .map(|path| path.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                )
            }
            AliasResolution::External => "external".to_owned(),
            AliasResolution::Unknown => "unknown".to_owned(),
        }
    }

    fn resolve(files: &[(&str, &str)], spec: &str) -> String {
        resolve_removing(files, &[], spec)
    }

    const TRANSPORT: (&str, &str) = (
        "src/net/transport.ts",
        "export class RequestFailure extends Error {}\n",
    );
    const PATHS: &str = r#"{ "compilerOptions": { "paths": { "@/*": ["./src/*"] } } }"#;

    /// paths の別名を、置換先のファイルに解く。baseUrl の有無で置換先の基点が変わる。
    #[test]
    fn resolves_paths_aliases_to_local_files() {
        for config in [
            r#"{ "compilerOptions": { "baseUrl": ".", "paths": { "@/*": ["./src/*"] } } }"#,
            PATHS,
            r#"{ "compilerOptions": { "baseUrl": "./src", "paths": { "@/*": ["./*"] } } }"#,
            // JSONC (コメントと末尾の comma) と BOM
            "\u{feff}{ // alias\n \"compilerOptions\": { \"paths\": { \"@/*\": [\"./src/*\",], }, }, }",
            // 相対でない指定子には rootDirs は効かない
            r#"{ "compilerOptions": { "rootDirs": ["src", "gen"], "paths": { "@/*": ["./src/*"] } } }"#,
        ] {
            assert_eq!(
                resolve(&[("tsconfig.json", config), TRANSPORT], "@/net/transport"),
                "local:src/net/transport.ts",
                "{config}"
            );
        }
        // 置換先がディレクトリなら index を解く
        assert_eq!(
            resolve(
                &[
                    ("tsconfig.json", PATHS),
                    ("src/net/index.ts", "export const x = 1;\n"),
                ],
                "@/net"
            ),
            "local:src/net/index.ts"
        );
        // node10 と bundler で選ぶ置換先が違っても (前の置換先の JS と後ろの置換先の TS)、
        // どちらも候補に入る
        assert_eq!(
            resolve(
                &[
                    (
                        "tsconfig.json",
                        r#"{ "compilerOptions": { "paths": { "x": ["./a", "./b"] } } }"#
                    ),
                    ("a.js", "export function helper() {}\n"),
                    ("b.ts", "export const remaining = 1;\n"),
                ],
                "x"
            ),
            "local:a.js,b.ts"
        );
    }

    /// 最優先のパターンだけを使い、その置換先が全滅すると native の解決へ進む (弱いパターンや
    /// baseUrl に進まない)。`#` は package.json の `imports` で中を指し得るので決めない。
    #[test]
    fn failed_best_pattern_falls_back_to_native_resolution() {
        let config = r##"{ "compilerOptions": { "baseUrl": ".", "paths": { "#failure": ["./missing"], "*": ["./src/net/transport"] } } }"##;
        assert_eq!(
            resolve(
                &[("tsconfig.json", config), TRANSPORT, ("#failure.ts", "")],
                "#failure"
            ),
            "unknown"
        );
        // 完全一致が無ければ最長の接頭辞のパターン
        let config = r#"{ "compilerOptions": { "paths": { "@/*": ["./missing/*"], "@/net/*": ["./src/net/*"] } } }"#;
        assert_eq!(
            resolve(&[("tsconfig.json", config), TRANSPORT], "@/net/transport"),
            "local:src/net/transport.ts"
        );
        // 依存の名前に当たる `*` のパターンが解けなければ、ルートの registry の依存は外部
        let config = r#"{ "compilerOptions": { "paths": { "*": ["./types/*"] } } }"#;
        let package = r#"{ "dependencies": { "lodash": "^4.0.0" } }"#;
        assert_eq!(
            resolve(
                &[("tsconfig.json", config), ("package.json", package)],
                "lodash"
            ),
            "external"
        );
        assert_eq!(
            resolve(
                &[
                    ("tsconfig.json", config),
                    ("package.json", package),
                    ("types/lodash.d.ts", "export {};\n"),
                ],
                "lodash"
            ),
            "local:types/lodash.d.ts"
        );
        // 削除したファイルが置換先にあって native の解決へ進むなら、削除前はそのファイルに
        // 解かれていたので決めない
        assert_eq!(
            resolve_removing(
                &[("tsconfig.json", config), ("package.json", package)],
                &["types/lodash.ts"],
                "lodash"
            ),
            "unknown"
        );
    }

    /// 完全一致のキーの置換先の `*` は置き換えない。空の捕獲は置き換えた形と置き換えない形の
    /// 両方を候補にする。
    #[test]
    fn star_substitution_follows_both_typescript_versions() {
        let config = r#"{ "compilerOptions": { "paths": { "a*": ["./x*", "./old"] } } }"#;
        assert_eq!(
            resolve(
                &[
                    ("tsconfig.json", config),
                    ("x.ts", "export const x = 1;\n"),
                    ("old.ts", "export const x = 1;\n"),
                ],
                "a"
            ),
            "local:old.ts,x.ts"
        );
        let config = r#"{ "compilerOptions": { "paths": { "exact": ["./lib/*"] } } }"#;
        assert_eq!(
            resolve(
                &[
                    ("tsconfig.json", config),
                    ("lib/.ts", "export const x = 1;\n")
                ],
                "exact"
            ),
            "unknown"
        );
        // 置き換えない解釈 (TS 6) が native の解決 (`#` の imports) へ進むなら、置き換えた解釈が
        // 生き残りのファイルに当たっても決めない
        let survivor = ("src/survivor.ts", "export class RequestFailure {}\n");
        assert_eq!(
            resolve(
                &[
                    (
                        "tsconfig.json",
                        r##"{ "compilerOptions": { "paths": { "#a*": ["./src/survivor*"] } } }"##
                    ),
                    survivor,
                ],
                "#a"
            ),
            "unknown"
        );
        // 同じ長さの接頭辞のパターンは先に書いたものだけを使う (後のパターンの当たりで native の
        // 解決を消さない)
        assert_eq!(
            resolve(
                &[
                    (
                        "tsconfig.json",
                        r##"{ "compilerOptions": { "paths": { "#*": ["./missing"], "#*a": ["./src/survivor"] } } }"##
                    ),
                    survivor,
                ],
                "#a"
            ),
            "unknown"
        );
        // 指定子から捕獲した `${configDir}` は展開しない (設定の値の先頭のものだけを展開する)
        assert_eq!(
            resolve(
                &[
                    (
                        "tsconfig.json",
                        r##"{ "compilerOptions": { "baseUrl": ".", "paths": { "#*": ["*"] } } }"##
                    ),
                    ("${configDir}/src/item.ts", "export const x = 1;\n"),
                    ("src/item.ts", "export const x = 1;\n"),
                ],
                "#${configDir}/src/item"
            ),
            "local:${configDir}/src/item.ts"
        );
    }

    /// 結果の種類。
    fn kind(resolution: &AliasResolution) -> &'static str {
        match resolution {
            AliasResolution::Local(_) => "local",
            AliasResolution::External => "external",
            AliasResolution::Unknown => "unknown",
        }
    }

    /// registry の依存は、参照ファイルの祖先 (ルートまで) の package.json に書いたときだけ外部
    /// (Issue #280)。祖先でないワークスペースのパッケージにだけ書いた依存と、名前の似たディレクトリ
    /// の依存は数えない。同じ解決器でどの順に解いても、参照ファイルのディレクトリごとに決める。
    #[test]
    fn registry_dependencies_of_ancestor_manifests_are_external() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            &[
                (
                    "package.json",
                    r#"{ "name": "mono", "private": true, "workspaces": ["packages/*"] }"#,
                ),
                (
                    "packages/ui/package.json",
                    r#"{ "name": "@example/ui", "dependencies": { "react": "19.0.0" }, "devDependencies": { "@types/react": "19.0.0" } }"#,
                ),
                (
                    "packages/ui-other/package.json",
                    r#"{ "name": "@example/ui-other" }"#,
                ),
                (
                    "packages/package.json",
                    r#"{ "dependencies": { "lodash": "^4.0.0" } }"#,
                ),
                (
                    "tools/package.json",
                    r#"{ "optionalDependencies": { "chalk": "^5.0.0" } }"#,
                ),
            ],
        );
        let cases = [
            ("packages/ui/src/tag.tsx", "react", "external"),
            ("packages/ui/tag.tsx", "@types/react", "external"),
            (
                "packages/ui/src/deep/view.tsx",
                "react/jsx-runtime",
                "external",
            ),
            // 祖先でないワークスペースのパッケージ・名前の似たディレクトリ・ルートのファイル
            ("packages/ui-other/src/tag.tsx", "react", "unknown"),
            ("packages/other/tag.tsx", "react", "unknown"),
            ("index.ts", "react", "unknown"),
            // 途中の祖先の package.json の依存
            ("packages/ui/src/tag.tsx", "lodash", "external"),
            ("packages/ui-other/src/tag.tsx", "lodash", "external"),
            ("index.ts", "lodash", "unknown"),
            ("tools/run.ts", "chalk", "external"),
            ("packages/ui/src/tag.tsx", "chalk", "unknown"),
        ];
        for reversed in [false, true] {
            let mut resolver = TsAliasResolver::load(dir.path().to_str().unwrap(), HashSet::new());
            let ordered: Vec<_> = if reversed {
                cases.iter().rev().collect()
            } else {
                cases.iter().collect()
            };
            for (ref_path, spec, expected) in ordered {
                assert_eq!(
                    kind(&resolver.resolve(ref_path, spec)),
                    *expected,
                    "{ref_path} {spec} (reversed: {reversed})"
                );
            }
        }
        // ほかの package.json の registry でない指定・同じ名前のパッケージ・上書きは、どこからも
        // 決めない
        for extra in [
            (
                "packages/legacy/package.json",
                r#"{ "dependencies": { "react": "file:../vendor/react" } }"#,
            ),
            ("packages/react/package.json", r#"{ "name": "react" }"#),
            (
                "package.json",
                r#"{ "name": "mono", "workspaces": ["packages/*"], "overrides": { "react": "link:./vendor/react" } }"#,
            ),
        ] {
            let dir = tempfile::tempdir().expect("tempdir");
            let mut files = vec![
                (
                    "package.json",
                    r#"{ "name": "mono", "workspaces": ["packages/*"] }"#,
                ),
                (
                    "packages/ui/package.json",
                    r#"{ "dependencies": { "react": "19.0.0" } }"#,
                ),
            ];
            match files.iter_mut().find(|(path, _)| *path == extra.0) {
                Some(entry) => entry.1 = extra.1,
                None => files.push(extra),
            }
            write(dir.path(), &files);
            let mut resolver = TsAliasResolver::load(dir.path().to_str().unwrap(), HashSet::new());
            assert_eq!(
                kind(&resolver.resolve("packages/ui/src/tag.tsx", "react")),
                "unknown",
                "{extra:?}"
            );
        }
    }

    /// 依存の名前でも、baseUrl の下にファイルがあればローカルに解かれる。ワークスペース・
    /// registry でない依存・typeRoots の下のパッケージは、node_modules の外を指し得るので決めない。
    #[test]
    fn dependency_names_are_external_only_without_local_overrides() {
        let package = r#"{ "dependencies": { "lodash": "^4.0.0", "ui": "workspace:*" } }"#;
        assert_eq!(resolve(&[("package.json", package)], "lodash"), "external");
        assert_eq!(
            resolve(
                &[
                    ("package.json", package),
                    (
                        "tsconfig.json",
                        r#"{ "compilerOptions": { "baseUrl": "./src" } }"#
                    ),
                    ("src/lodash.ts", "export function debounce() {}\n"),
                ],
                "lodash"
            ),
            "local:src/lodash.ts"
        );
        assert_eq!(resolve(&[("package.json", package)], "ui"), "unknown");
        assert_eq!(
            resolve(
                &[
                    ("package.json", package),
                    ("packages/lodash/package.json", r#"{ "name": "lodash" }"#),
                ],
                "lodash"
            ),
            "unknown"
        );
        assert_eq!(
            resolve(
                &[(
                    "package.json",
                    r#"{ "dependencies": { "lodash": "^4.0.0" }, "overrides": { "lodash": "file:./vendor/lodash" } }"#
                )],
                "lodash"
            ),
            "unknown"
        );
        assert_eq!(
            resolve(
                &[
                    ("package.json", package),
                    (
                        "tsconfig.json",
                        r#"{ "compilerOptions": { "typeRoots": ["./types"] } }"#
                    ),
                    (
                        "types/lodash/package.json",
                        r#"{ "types": "../../src/old.ts" }"#
                    ),
                ],
                "lodash"
            ),
            "unknown"
        );
        // typeRoots の下の宣言ファイルを消したなら、削除前はそこへ解かれていた
        let type_roots = r#"{ "compilerOptions": { "moduleResolution": "node10", "typeRoots": ["./types", "./node_modules/@types"] } }"#;
        assert_eq!(
            resolve_removing(
                &[("package.json", package), ("tsconfig.json", type_roots)],
                &["types/lodash.d.ts"],
                "lodash"
            ),
            "unknown"
        );
        // node_modules の下の typeRoots は registry の型なので、外部パッケージのまま
        assert_eq!(
            resolve(
                &[
                    ("package.json", package),
                    ("tsconfig.json", type_roots),
                    ("node_modules/@types/lodash/index.d.ts", "export {};\n"),
                ],
                "lodash"
            ),
            "external"
        );
        // 大文字小文字を区別しないファイルシステムでは `TSConfig.json` も設定として読まれる
        assert_eq!(
            resolve_removing(
                &[
                    ("package.json", package),
                    (
                        "TSConfig.json",
                        r#"{ "compilerOptions": { "paths": { "lodash": ["./src/deleted"] } } }"#
                    ),
                ],
                &["src/deleted.ts"],
                "lodash"
            ),
            "unknown"
        );
        // パッケージの外へ出る `..` を含む指定子は、依存の名前でも外部と言えない
        assert_eq!(
            resolve(&[("package.json", package)], "lodash/../../src/deleted"),
            "unknown"
        );
        // 依存に無いパッケージと、設定がどれも解かない別名は決めない
        assert_eq!(resolve(&[("package.json", package)], "react"), "unknown");
        assert_eq!(
            resolve(&[("package.json", package)], "@/net/transport"),
            "unknown"
        );
    }

    /// classic になり得る設定は、参照ファイルの祖先のディレクトリの同名のファイルにも解く。
    #[test]
    fn classic_resolution_searches_ancestor_directories() {
        let package = r#"{ "dependencies": { "dep": "^1.0.0" } }"#;
        let classic = r#"{ "compilerOptions": { "module": "ESNext" } }"#;
        assert_eq!(
            resolve(
                &[
                    ("package.json", package),
                    ("tsconfig.json", classic),
                    ("src/dep.ts", "export const x = 1;\n"),
                ],
                "dep"
            ),
            "local:src/dep.ts"
        );
        assert_eq!(
            resolve(
                &[("package.json", package), ("tsconfig.json", classic)],
                "dep"
            ),
            "external"
        );
        assert_eq!(
            resolve_removing(
                &[("package.json", package), ("tsconfig.json", classic)],
                &["src/app/dep.ts"],
                "dep"
            ),
            "unknown"
        );
        // node10 は祖先を探さない
        assert_eq!(
            resolve(
                &[
                    ("package.json", package),
                    (
                        "tsconfig.json",
                        r#"{ "compilerOptions": { "module": "commonjs" } }"#
                    ),
                    ("src/dep.ts", "export const x = 1;\n"),
                ],
                "dep"
            ),
            "external"
        );
    }

    /// 別名を持たない設定 (Vite の tsconfig.node.json など) があっても、別名を持つ設定の解決先を
    /// 使う。どれかの設定が読めなければ決めない。
    #[test]
    fn combines_every_project_config() {
        let node =
            r#"{ "compilerOptions": { "module": "ESNext", "moduleResolution": "bundler" } }"#;
        assert_eq!(
            resolve(
                &[
                    ("tsconfig.app.json", PATHS),
                    ("tsconfig.node.json", node),
                    (
                        "tsconfig.json",
                        r#"{ "files": [], "references": [{ "path": "./tsconfig.app.json" }, { "path": "./tsconfig.node.json" }] }"#
                    ),
                    TRANSPORT,
                ],
                "@/net/transport"
            ),
            "local:src/net/transport.ts"
        );
        assert_eq!(
            resolve(
                &[
                    ("tsconfig.json", PATHS),
                    ("tsconfig.broken.json", "{ \"compilerOptions\": "),
                    TRANSPORT,
                ],
                "@/net/transport"
            ),
            "unknown"
        );
        // 別名を持つ設定どうしで解決先が違えば、両方を候補にする
        assert_eq!(
            resolve(
                &[
                    ("tsconfig.json", PATHS),
                    (
                        "web/tsconfig.json",
                        r#"{ "compilerOptions": { "paths": { "@/*": ["./lib/*"] } } }"#
                    ),
                    TRANSPORT,
                    (
                        "web/lib/net/transport.ts",
                        "export class RequestFailure {}\n"
                    ),
                ],
                "@/net/transport"
            ),
            "local:src/net/transport.ts,web/lib/net/transport.ts"
        );
    }

    /// extends の継承: 子の paths は全体を上書きし、paths の基点は実効の baseUrl、無ければ paths を
    /// 書いた設定のディレクトリ。`${configDir}` は使う設定のディレクトリ。
    #[test]
    fn follows_extends_like_typescript() {
        let base = r#"{ "compilerOptions": { "paths": { "@/*": ["./src/*"] } } }"#;
        assert_eq!(
            resolve(
                &[
                    ("configs/base.json", base),
                    ("tsconfig.json", r#"{ "extends": "./configs/base.json" }"#),
                    (
                        "configs/src/net/transport.ts",
                        "export class RequestFailure {}\n"
                    ),
                    TRANSPORT,
                ],
                "@/net/transport"
            ),
            "local:configs/src/net/transport.ts"
        );
        assert_eq!(
            resolve(
                &[
                    ("configs/base.json", base),
                    (
                        "tsconfig.json",
                        r#"{ "extends": "./configs/base", "compilerOptions": { "baseUrl": "." } }"#
                    ),
                    TRANSPORT,
                ],
                "@/net/transport"
            ),
            "local:src/net/transport.ts"
        );
        assert_eq!(
            resolve(
                &[
                    (
                        "configs/base.json",
                        r#"{ "compilerOptions": { "paths": { "@/*": ["${configDir}/src/*"] } } }"#
                    ),
                    ("tsconfig.json", r#"{ "extends": ["./configs/base.json"] }"#),
                    TRANSPORT,
                ],
                "@/net/transport"
            ),
            "local:src/net/transport.ts"
        );
        // node_modules のパッケージの設定 (exports が無いもの) を辿る。見つからなければ決めない
        let extending = r#"{ "extends": "@tsconfig/strictest/tsconfig.json", "compilerOptions": { "paths": { "@/*": ["./src/*"] } } }"#;
        assert_eq!(
            resolve(
                &[
                    ("tsconfig.json", extending),
                    (
                        "node_modules/@tsconfig/strictest/tsconfig.json",
                        r#"{ "compilerOptions": { "strict": true } }"#
                    ),
                    TRANSPORT,
                ],
                "@/net/transport"
            ),
            "local:src/net/transport.ts"
        );
        assert_eq!(
            resolve(
                &[("tsconfig.json", extending), TRANSPORT],
                "@/net/transport"
            ),
            "unknown"
        );
    }

    /// node16 / nodenext で当たる別名、classic で index にしか当たらない別名、package.json の
    /// ディレクトリ、解けない設定値は決めない。JS のファイルにしか当たらない別名は、native の
    /// 解決がリポジトリへ届かないときだけ候補にする。
    #[test]
    fn rejects_resolutions_that_depend_on_the_mode_or_unread_inputs() {
        for config in [
            r#"{ "compilerOptions": { "module": "Node16", "paths": { "@/*": ["./src/*"] } } }"#,
            r#"{ "compilerOptions": { "moduleResolution": "nodenext", "paths": { "@/*": ["./src/*"] } } }"#,
            r#"{ "compilerOptions": { "moduleSuffixes": [".ios", ""], "paths": { "@/*": ["./src/*"] } } }"#,
            r#"{ "compilerOptions": { "paths": { "@/*/x": ["./src/*"] } } }"#,
            r#"{ "compilerOptions": { "paths": { "@/*": [] } } }"#,
            r#"{ "compilerOptions": { "moduleResolution": "hybrid", "paths": { "@/*": ["./src/*"] } } }"#,
        ] {
            assert_eq!(
                resolve(&[("tsconfig.json", config), TRANSPORT], "@/net/transport"),
                "unknown",
                "{config}"
            );
        }
        let classic =
            r#"{ "compilerOptions": { "module": "ESNext", "paths": { "@/*": ["./src/*"] } } }"#;
        assert_eq!(
            resolve(&[("tsconfig.json", classic), TRANSPORT], "@/net/transport"),
            "local:src/net/transport.ts"
        );
        assert_eq!(
            resolve(
                &[
                    ("tsconfig.json", classic),
                    ("src/net/index.ts", "export const x = 1;\n")
                ],
                "@/net"
            ),
            "unknown"
        );
        assert_eq!(
            resolve(
                &[
                    ("tsconfig.json", PATHS),
                    ("src/net/package.json", r#"{ "types": "./x.d.ts" }"#),
                    ("src/net/index.ts", "export const x = 1;\n"),
                ],
                "@/net"
            ),
            "unknown"
        );
        assert_eq!(
            resolve(
                &[
                    ("tsconfig.json", PATHS),
                    ("src/net/transport.js", "export class RequestFailure {}\n"),
                ],
                "@/net/transport"
            ),
            "local:src/net/transport.js"
        );
        assert_eq!(
            resolve(
                &[
                    (
                        "tsconfig.json",
                        r##"{ "compilerOptions": { "paths": { "#x": ["./src/x"] } } }"##
                    ),
                    ("src/x.js", "export const x = 1;\n"),
                ],
                "#x"
            ),
            "unknown"
        );
        // `/` で終わる置換先はファイルを探さない
        assert_eq!(
            resolve(
                &[
                    (
                        "tsconfig.json",
                        r#"{ "compilerOptions": { "paths": { "@net": ["./src/net/transport/"] } } }"#
                    ),
                    TRANSPORT,
                ],
                "@net"
            ),
            "unknown"
        );
    }

    /// 宣言ファイル・拡張子の読み替え・任意の拡張子の宣言と、削除済みのファイルも候補に入る
    /// (存在しない候補を残す)。
    #[test]
    fn candidates_include_declarations_and_missing_files() {
        for (spec_path, expected) in [
            ("errors", "src/server/errors.d.ts"),
            ("errors", "src/server/errors.ts"),
            ("errors", "src/server/errors/index.ts"),
            ("errors.js", "src/server/errors.js.ts"),
            ("errors.js", "src/server/errors.tsx"),
            ("errors.ts", "src/server/errors.tsx"),
            ("errors.d.ts", "src/server/errors.ts"),
            ("errors.css", "src/server/errors.d.css.ts"),
            ("errors.mjs", "src/server/errors.d.mts"),
        ] {
            let candidates = module_file_candidates(&Path::new("/r/src/server").join(spec_path))
                .into_iter()
                .chain(RESOLVED_EXTENSIONS.iter().map(|e| {
                    Path::new("/r/src/server")
                        .join(spec_path)
                        .join(format!("index{e}"))
                }))
                .filter_map(|path| {
                    path.strip_prefix("/r")
                        .ok()
                        .and_then(|path| path.to_str())
                        .map(normalize_workspace_separators)
                })
                .collect::<HashSet<_>>();
            assert!(
                candidates.contains(expected),
                "{spec_path}: {expected} not in {candidates:?}"
            );
        }
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            &[
                ("tsconfig.json", PATHS),
                (
                    "src/server/errors.d.ts",
                    "export interface RequestFailure {}\n",
                ),
                ("src/server/errors.js", "export class RequestFailure {}\n"),
            ],
        );
        let mut resolver = TsAliasResolver::load(dir.path().to_str().unwrap(), HashSet::new());
        let AliasResolution::Local(candidates) =
            resolver.resolve("src/app/use.ts", "@/server/errors")
        else {
            panic!("local");
        };
        for expected in [
            "src/server/errors.d.ts",
            "src/server/errors.js",
            "src/server/errors.ts",
            "src/server/errors/index.ts",
        ] {
            assert!(candidates.contains(expected), "{expected}: {candidates:?}");
        }
    }

    /// 置換先の途中に symlink があれば、字句の上のパスで比べられないので決めない。
    #[cfg(unix)]
    #[test]
    fn symlinked_directories_are_not_resolved() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            &[
                (
                    "tsconfig.json",
                    r#"{ "compilerOptions": { "paths": { "@/*": ["./src/link/*"] } } }"#,
                ),
                ("src/server/errors.ts", "export class RequestFailure {}\n"),
            ],
        );
        std::os::unix::fs::symlink(dir.path().join("src/server"), dir.path().join("src/link"))
            .unwrap();
        let mut resolver = TsAliasResolver::load(dir.path().to_str().unwrap(), HashSet::new());
        assert!(matches!(
            resolver.resolve("src/app/use.ts", "@/errors"),
            AliasResolution::Unknown
        ));
    }

    /// symlink を経る typeRoots でも、削除した宣言ファイルを正規化した位置で見つける。
    #[cfg(unix)]
    #[test]
    fn linked_type_roots_are_compared_where_they_point() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            &[
                ("package.json", r#"{ "dependencies": { "pkg": "1.0.0" } }"#),
                (
                    "tsconfig.json",
                    r#"{ "compilerOptions": { "moduleResolution": "node10", "typeRoots": ["./linked-types"] } }"#,
                ),
                ("types/other.d.ts", "export {};\n"),
            ],
        );
        std::os::unix::fs::symlink(dir.path().join("types"), dir.path().join("linked-types"))
            .unwrap();
        let mut resolver = TsAliasResolver::load(
            dir.path().to_str().unwrap(),
            HashSet::from(["types/pkg.d.ts".to_owned()]),
        );
        assert!(matches!(
            resolver.resolve("src/use.ts", "pkg"),
            AliasResolution::Unknown
        ));
        let mut resolver = TsAliasResolver::load(dir.path().to_str().unwrap(), HashSet::new());
        assert!(matches!(
            resolver.resolve("src/use.ts", "pkg"),
            AliasResolution::External
        ));
    }

    /// 設定が無ければ、ルートの registry の依存だけを外部とする。
    #[test]
    fn without_configs_only_root_dependencies_are_external() {
        assert_eq!(resolve(&[], "lodash"), "unknown");
        assert_eq!(
            resolve(
                &[(
                    "package.json",
                    r#"{ "devDependencies": { "@scope/pkg": "npm:other@1" } }"#
                )],
                "@scope/pkg/sub"
            ),
            "external"
        );
        assert_eq!(resolve(&[], "/abs/path"), "unknown");
    }
}
