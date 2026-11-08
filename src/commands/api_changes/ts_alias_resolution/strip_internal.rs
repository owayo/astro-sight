//! ファイルに当たる TypeScript の `stripInternal` の設定 (Issue #274)。
//!
//! ファイルから近いディレクトリの順に、tsconfig*.json / jsconfig*.json を見る。ファイルを入力に
//! 含む設定 (`files`・`include`・`exclude` の実効値。`extends` で継いだものも。`exclude` が無ければ
//! `outDir`・`declarationDir` を除く) のあるディレクトリで決め、そのどれかが実効で `stripInternal` を
//! 有効にすれば有効とする (宣言は `tsconfig.build.json` のような別名の設定で出すことが多い)。
//! 設定は入力から import で取り込んだファイルの宣言も出すので、TypeScript と同じ解決で取り込むと
//! 確かめられたファイルも入力と同じく数える (Issue #288。[`reach`])。ファイルを入力に含まず、
//! 取り込まないと確かめられた設定は、判定を止めず、数えもしない。取り込むかを確かめられない設定は、
//! 消えるとはみなさず (宣言ファイルに残る公開の宣言の削除を見逃さない)、判定を決められない
//! ([`StripState::Unknown`]) ものとする。
//!
//! `stripInternal` の実効値は、設定自身の値、配列の `extends` の後ろの設定、前の設定の順に決まり、
//! `null` は既定 (無効) に戻す。読めない・解けない設定は、ファイルを含み得るものとして扱い、
//! `stripInternal` を有効にするとはみなさない。`extends` は解析するディレクトリの外も辿る。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use serde_json::{Map, Value};

use super::config_fs::{ConfigFs, Listing};
use super::{
    ConfigLoader, MAX_ANCESTORS, MAX_CHAIN_DEPTH, is_config_name, normalize_lexically, option_path,
};

mod reach;
pub(crate) use reach::imports_differ;

/// TypeScript がワイルドカードで辿らないディレクトリ。
const PACKAGE_FOLDERS: [&str; 3] = ["node_modules", "bower_components", "jspm_packages"];

/// ファイルに当たる `stripInternal` の判定 (Issue #287)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StripState {
    /// ファイルを入力に含む設定のどれかが有効にする。
    Strips,
    /// 有効にする設定が無い (設定が無いことも含む)。
    Keeps,
    /// 決められない (設定やディレクトリの一覧を読めない・解けない、入力に含むか分からない設定が
    /// 有効にする)。
    Unknown,
}

pub(crate) struct StripInternalConfigs {
    root: Option<PathBuf>,
    loader: ConfigLoader,
    /// ディレクトリ → その設定 (一覧を読めなければ None)。
    directories: HashMap<PathBuf, Option<Vec<PathBuf>>>,
    /// 設定 → 入力 (読めない・解けなければ None)。
    inputs: HashMap<PathBuf, Option<Inputs>>,
    /// 設定 → `stripInternal` の実効値 (読めない・解けなければ None)。
    strips: HashMap<PathBuf, Option<bool>>,
    /// 設定 → 入力から取り込みを辿ったファイル (Issue #288)。
    graphs: HashMap<PathBuf, Rc<reach::Graph>>,
    /// グラフで共有する読み取り (ソースの取り込み・存在・package.json)。
    reach: reach::ReachCache,
}

/// 判定に数える設定 1 つ。
struct Candidate {
    config: PathBuf,
    membership: Membership,
    setting: Option<bool>,
}

/// 設定 1 つがファイルを含むときの判定 ((有効にするか, [`StripState`]))。
fn outcome(membership: Membership, setting: Option<bool>) -> (bool, StripState) {
    let state = match (membership, setting) {
        (Membership::Includes, Some(true)) => StripState::Strips,
        (_, Some(false)) => StripState::Keeps,
        _ => StripState::Unknown,
    };
    (setting == Some(true), state)
}

/// 同じディレクトリの設定の判定を合わせる (どれかが有効にすれば有効)。
fn combine(outcomes: &[(bool, StripState)]) -> Option<(bool, StripState)> {
    outcomes
        .iter()
        .copied()
        .reduce(|(strips, state), (other, other_state)| {
            (strips || other, stronger(state, other_state))
        })
}

/// 合わせた判定: 有効にするものがあれば有効、無ければ決められないものがあれば決められない。
fn stronger(a: StripState, b: StripState) -> StripState {
    let rank = |state: StripState| match state {
        StripState::Keeps => 0,
        StripState::Unknown => 1,
        StripState::Strips => 2,
    };
    if rank(a) >= rank(b) { a } else { b }
}

impl StripInternalConfigs {
    pub(crate) fn new(dir: &str) -> Self {
        Self::with_fs(dir, ConfigFs::Current)
    }

    /// 設定をファイルシステム `fs` (今のものか base の版) から読む。
    pub(crate) fn with_fs(dir: &str, fs: ConfigFs) -> Self {
        Self {
            root: std::path::absolute(Path::new(dir))
                .ok()
                .and_then(|path| path.canonicalize().ok()),
            loader: ConfigLoader {
                fs,
                ..ConfigLoader::default()
            },
            directories: HashMap::new(),
            inputs: HashMap::new(),
            strips: HashMap::new(),
            graphs: HashMap::new(),
            reach: reach::ReachCache::default(),
        }
    }

    /// `path` (`dir` 相対) のファイルの宣言を出し得る設定が、`stripInternal` を実効で有効に
    /// するか。削除したファイルのディレクトリが無くても、祖先の設定を見る。読めない・解けない
    /// 設定は、ファイルを含み得るものとして扱い、有効にするとはみなさない。
    pub(crate) fn strips(&mut self, path: &str) -> bool {
        self.evaluate(path).0
    }

    /// [`Self::strips`] を、決められないときを分けて返す。設定だけの変更で旧新を比べ直すときは、
    /// 決められない側があれば確定した差分にしない。
    pub(crate) fn strips_state(&mut self, path: &str) -> StripState {
        self.evaluate(path).1
    }

    /// ([`Self::strips`] の値, [`Self::strips_state`] の値)。
    ///
    /// 入力に含まない設定は、取り込みで届くと確かめられれば入力と同じく数え、届かないと確かめ
    /// られれば数えない (Issue #288)。届くかを確かめられない設定は、届く場合と届かない場合の
    /// どちらの判定もあり得るものとし、どちらでも有効にするときだけ有効、判定が分かれれば決め
    /// られないとする。取り込みを辿るのは、届けば判定が変わり得るときだけ。
    fn evaluate(&mut self, path: &str) -> (bool, StripState) {
        let Some(file) = self.root.as_ref().map(|root| root.join(path)) else {
            return (false, StripState::Unknown);
        };
        let Some(start) = file.parent().map(Path::to_path_buf) else {
            return (false, StripState::Unknown);
        };
        let target = components(&file);
        // 近いディレクトリから、ファイルを入力に含む (か分からない) 設定のあるディレクトリまで。
        let mut levels: Vec<Vec<Candidate>> = Vec::new();
        for (depth, directory) in start.ancestors().enumerate() {
            if depth > MAX_ANCESTORS {
                return (false, StripState::Unknown);
            }
            let Some(configs) = self.directory_configs(directory) else {
                return (false, StripState::Unknown);
            };
            let mut level = Vec::new();
            for config in configs {
                let membership = self.membership(&config, &target);
                let setting = self.config_strips(&config);
                level.push(Candidate {
                    config,
                    membership,
                    setting,
                });
            }
            let decides = level
                .iter()
                .any(|candidate| !matches!(candidate.membership, Membership::Excludes));
            levels.push(level);
            if decides {
                break;
            }
        }
        // 入力だけで決めた判定。どの設定がファイルを含むとしても同じ判定になるなら、取り込みを
        // 辿らない。
        let baseline = levels
            .last()
            .and_then(|level| {
                let outcomes: Vec<_> = level
                    .iter()
                    .filter(|candidate| !matches!(candidate.membership, Membership::Excludes))
                    .map(|candidate| outcome(candidate.membership, candidate.setting))
                    .collect();
                combine(&outcomes)
            })
            .unwrap_or((false, StripState::Keeps));
        let alone = |candidate: &Candidate| {
            let membership = match candidate.membership {
                Membership::Excludes => Membership::Includes,
                membership => membership,
            };
            outcome(membership, candidate.setting)
        };
        if levels
            .iter()
            .flatten()
            .all(|candidate| alone(candidate) == baseline)
        {
            return baseline;
        }
        let canonical = self
            .loader
            .fs
            .canonicalize(&file)
            .unwrap_or_else(|| file.clone());
        let mut reaches = HashMap::new();
        for candidate in levels.iter().flatten() {
            if matches!(candidate.membership, Membership::Excludes)
                && !reaches.contains_key(&candidate.config)
            {
                let reach = self.graph(&candidate.config).reach(&canonical);
                reaches.insert(candidate.config.clone(), reach);
            }
        }
        // 届くかを確かめられない設定ごとに、届く場合と届かない場合の判定を集める。
        let mut states = Vec::new();
        let mut strips = true;
        let mut settled = false;
        for level in &levels {
            let mut definite = Vec::new();
            let mut maybe = Vec::new();
            for candidate in level {
                match candidate.membership {
                    Membership::Excludes => match reaches.get(&candidate.config) {
                        Some(reach::Reach::Reached) => {
                            definite.push(outcome(Membership::Includes, candidate.setting));
                        }
                        Some(reach::Reach::Unknown) => {
                            maybe.push(outcome(Membership::Includes, candidate.setting));
                        }
                        _ => {}
                    },
                    membership => definite.push(outcome(membership, candidate.setting)),
                }
            }
            if let Some(decided) = combine(&definite) {
                states.push(decided.1);
                states.extend(maybe.iter().map(|(_, state)| stronger(decided.1, *state)));
                strips &= decided.0;
                settled = true;
                break;
            }
            for (maybe_strips, state) in maybe {
                states.push(state);
                strips &= maybe_strips;
            }
        }
        if !settled {
            states.push(StripState::Keeps);
            strips = false;
        }
        let state = if states.windows(2).all(|pair| pair[0] == pair[1]) {
            states[0]
        } else {
            StripState::Unknown
        };
        (strips, state)
    }

    /// 設定 `config` の入力から取り込みを辿ったファイル (初めて要るときに作る)。
    fn graph(&mut self, config: &Path) -> Rc<reach::Graph> {
        if let Some(graph) = self.graphs.get(config) {
            return Rc::clone(graph);
        }
        let options = reach::Options::read(&mut self.loader, config);
        let graph = match (self.inputs.get(config).and_then(Option::as_ref), options) {
            (Some(inputs), Some(options)) => {
                reach::build(&self.loader.fs, inputs, &options, &mut self.reach)
            }
            _ => reach::Graph::unknown(),
        };
        let graph = Rc::new(graph);
        self.graphs.insert(config.to_path_buf(), Rc::clone(&graph));
        graph
    }

    fn directory_configs(&mut self, directory: &Path) -> Option<Vec<PathBuf>> {
        let fs = &self.loader.fs;
        self.directories
            .entry(directory.to_path_buf())
            .or_insert_with(|| read_directory_configs(fs, directory))
            .clone()
    }

    fn membership(&mut self, config: &Path, target: &[String]) -> Membership {
        let loader = &mut self.loader;
        let inputs = self.inputs.entry(config.to_path_buf()).or_insert_with(|| {
            let raw = loader.raw_inputs(config, 0, &mut Vec::new())?;
            let leaf = config.parent()?;
            // `exclude` をどこにも書いていなければ、TypeScript は `outDir`・`declarationDir` を
            // 除く。
            let outputs = if raw.exclude.is_none() {
                loader.output_directories(config, leaf)?
            } else {
                Vec::new()
            };
            Inputs::resolve(raw, leaf, &outputs)
        });
        match inputs {
            Some(inputs) => inputs.membership(target),
            None => Membership::Unknown,
        }
    }

    /// 設定の `stripInternal` の実効値。読めない・解けなければ None。
    fn config_strips(&mut self, config: &Path) -> Option<bool> {
        if let Some(&strips) = self.strips.get(config) {
            return strips;
        }
        let strips = self
            .loader
            .strip_setting(config, 0, &mut Vec::new())
            .map(|setting| setting == Setting::Value(true));
        self.strips.insert(config.to_path_buf(), strips);
        strips
    }
}

/// `directory` の tsconfig*.json / jsconfig*.json (正規化したパス)。ディレクトリが無ければ空
/// (削除したファイルのディレクトリ)、一覧を読めなければ None。
fn read_directory_configs(fs: &ConfigFs, directory: &Path) -> Option<Vec<PathBuf>> {
    let names = match fs.read_dir(directory) {
        Listing::Entries(names) => names,
        Listing::Missing => return Some(Vec::new()),
        Listing::Unreadable => return None,
    };
    let mut configs = Vec::new();
    for name in names {
        if is_config_name(&name)
            && let Some(canonical) = fs.regular_file(&directory.join(&name))
        {
            configs.push(canonical);
        }
    }
    configs.sort();
    Some(configs)
}

/// 設定がファイルを入力に含むか。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Membership {
    Includes,
    Excludes,
    /// 読めない・解けない。
    Unknown,
}

/// `stripInternal` の、継承を辿った値。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Setting {
    /// どこにも書いていない。
    Unset,
    /// `null` で既定に戻した。
    Reset,
    Value(bool),
}

/// `compilerOptions` の値の、継承を辿った結果。
#[derive(Clone, Debug, PartialEq)]
enum OptionSetting {
    /// どこにも書いていない。
    Unset,
    /// `null` で既定に戻した。
    Reset,
    /// 値と、それを書いた設定のディレクトリ。
    Value(Value, PathBuf),
}

/// 書いたままの入力の指定と、それを書いた設定のディレクトリ。`None` は書いていない。
#[derive(Default)]
struct RawInputs {
    files: Option<Vec<(String, PathBuf)>>,
    include: Option<Vec<(String, PathBuf)>>,
    exclude: Option<Vec<(String, PathBuf)>>,
}

/// 設定の入力 (解いたパスの区切りの列)。
struct Inputs {
    files: Vec<Vec<String>>,
    include: Vec<Vec<String>>,
    exclude: Vec<Vec<String>>,
}

impl Inputs {
    /// TypeScript と同じく、`files` も `include` も書いていなければ `include` は設定のある
    /// ディレクトリの `**/*`、`files` だけなら `include` は無い。`exclude` が無ければ出力の
    /// ディレクトリ `outputs` (`outDir`・`declarationDir`) を除く。`${configDir}` は入力を決める
    /// 設定 (`leaf`) のディレクトリで解く。
    fn resolve(raw: RawInputs, leaf: &Path, outputs: &[PathBuf]) -> Option<Self> {
        let files = match &raw.files {
            Some(specs) => specs
                .iter()
                .map(|(text, directory)| Some(components(&spec_path(text, directory, leaf)?)))
                .collect::<Option<_>>()?,
            None => Vec::new(),
        };
        let include = match &raw.include {
            Some(specs) => patterns(specs, leaf, false)?,
            None if raw.files.is_none() => {
                patterns(&[("**/*".to_owned(), leaf.to_path_buf())], leaf, false)?
            }
            None => Vec::new(),
        };
        let exclude = match &raw.exclude {
            Some(specs) => patterns(specs, leaf, true)?,
            // 出力のディレクトリは、その下の全てを除く (`exclude` のパターンはディレクトリの下にも
            // 当たる)。
            None => outputs.iter().map(|output| components(output)).collect(),
        };
        Some(Self {
            files,
            include,
            exclude,
        })
    }

    /// `include` に当たり、`exclude` に当たらないか (TypeScript がディレクトリを辿って入力に
    /// 列挙するファイル。拡張子は呼び出し側で見る)。
    fn wildcard_includes(&self, target: &[String]) -> bool {
        self.include
            .iter()
            .any(|pattern| pattern_matches(pattern, target, false))
            && !self.excludes(target)
    }

    /// `exclude` に当たるか (ディレクトリなら、その下の全て)。
    fn excludes(&self, target: &[String]) -> bool {
        self.exclude
            .iter()
            .any(|pattern| pattern_matches(pattern, target, true))
    }

    /// ディレクトリ `directory` の下に、`include` に当たり得るファイルがあるか。
    fn may_descend(&self, directory: &[String]) -> bool {
        self.include
            .iter()
            .any(|pattern| pattern_may_descend(pattern, directory))
    }

    fn membership(&self, target: &[String]) -> Membership {
        let listed = self.files.iter().any(|file| {
            file.len() == target.len()
                && file
                    .iter()
                    .zip(target)
                    .all(|(file, target)| same_name(file, target))
        });
        let included = self
            .include
            .iter()
            .any(|pattern| pattern_matches(pattern, target, false))
            && !self
                .exclude
                .iter()
                .any(|pattern| pattern_matches(pattern, target, true));
        if listed || included {
            Membership::Includes
        } else {
            Membership::Excludes
        }
    }
}

/// 設定に書いたパス `text` を、書いた設定のディレクトリ `directory` か `${configDir}` (`leaf`)
/// から解く。
fn spec_path(text: &str, directory: &Path, leaf: &Path) -> Option<PathBuf> {
    normalize_lexically(&option_path(text, directory, leaf)?)
}

/// `include` / `exclude` のパターン。最後の区切りに `.`・`*`・`?` が無ければディレクトリとして
/// その下の全て (`/**/*`) にする。`include` の `**` で終わるパターンは TypeScript と同じく使わない。
fn patterns(specs: &[(String, PathBuf)], leaf: &Path, exclude: bool) -> Option<Vec<Vec<String>>> {
    let mut patterns = Vec::new();
    for (text, directory) in specs {
        let mut pattern = components(&spec_path(text, directory, leaf)?);
        let last = pattern.last()?;
        if !last.contains(['.', '*', '?']) {
            pattern.extend(["**".to_owned(), "*".to_owned()]);
        } else if !exclude && last == "**" {
            continue;
        }
        patterns.push(pattern);
    }
    Some(patterns)
}

/// パスの区切り (Windows の接頭辞と根を含む)。
fn components(path: &Path) -> Vec<String> {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect()
}

/// TypeScript の `include` / `exclude` のパターンが `target` に当たるか。`include` の `**` は
/// `.` で始まるディレクトリとパッケージのディレクトリを辿らない。`exclude` はディレクトリの下の
/// ファイルにも当たる。
fn pattern_matches(pattern: &[String], target: &[String], exclude: bool) -> bool {
    let Some((first, rest)) = pattern.split_first() else {
        return exclude || target.is_empty();
    };
    if first == "**" {
        if pattern_matches(rest, target, exclude) {
            return true;
        }
        for (index, directory) in target.iter().enumerate() {
            if !exclude && (directory.starts_with('.') || is_package_folder(directory)) {
                return false;
            }
            if pattern_matches(rest, &target[index + 1..], exclude) {
                return true;
            }
        }
        return false;
    }
    let Some((name, target_rest)) = target.split_first() else {
        return false;
    };
    component_matches(first, name, exclude, rest.is_empty())
        && pattern_matches(rest, target_rest, exclude)
}

/// `include` のパターン `pattern` が、ディレクトリ `directory` の下のファイルに当たり得るか
/// ([`pattern_matches`] と同じく、`**` は `.` で始まるディレクトリとパッケージのディレクトリを辿ら
/// ない)。
fn pattern_may_descend(pattern: &[String], directory: &[String]) -> bool {
    let Some((name, below)) = directory.split_first() else {
        return !pattern.is_empty();
    };
    let Some((first, rest)) = pattern.split_first() else {
        return false;
    };
    if first == "**" {
        if pattern_may_descend(rest, directory) {
            return true;
        }
        if name.starts_with('.') || is_package_folder(name) {
            return false;
        }
        return pattern_may_descend(pattern, below);
    }
    component_matches(first, name, false, false) && pattern_may_descend(rest, below)
}

/// パターンの区切り 1 つが名前に当たるか。`include` では、ワイルドカードのある区切りはパッケージの
/// ディレクトリに当たらず、先頭の `*`・`?` は `.` で始まる名前の先頭に当たらない。`include` の
/// 最後の区切り (`last`) の `*` は、TypeScript と同じく末尾の `.min.js` の `.` に当たらない
/// (`*.js` は `a.min.js` に当たらず、`*.min.js` は当たる)。
fn component_matches(pattern: &str, name: &str, exclude: bool, last: bool) -> bool {
    if !pattern.contains(['*', '?']) {
        return same_name(pattern, name);
    }
    let files = !exclude && last;
    if !exclude {
        if is_package_folder(name) {
            return false;
        }
        if name.starts_with('.') {
            return match pattern.strip_prefix('*') {
                Some(rest) => wildcard_matches(rest, name, files),
                None => !pattern.starts_with('?') && wildcard_matches(pattern, name, files),
            };
        }
    }
    wildcard_matches(pattern, name, files)
}

/// `*` (任意の文字列) と `?` (任意の 1 文字) のパターンが、名前の全体に当たるか。`min_js` なら、
/// `*` は末尾の `.min.js` の `.` に当たらない。
fn wildcard_matches(pattern: &str, name: &str, min_js: bool) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    let (mut p, mut n) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some((p, n));
                p += 1;
            }
            Some(&expected) if expected == '?' || same_char(expected, name[n]) => {
                p += 1;
                n += 1;
            }
            _ => match star {
                Some((_, star_n))
                    if min_js
                        && name.len() - star_n == ".min.js".len()
                        && name[star_n..]
                            .iter()
                            .zip(".min.js".chars())
                            .all(|(actual, expected)| same_char(expected, *actual)) =>
                {
                    return false;
                }
                Some((star_p, star_n)) => {
                    star = Some((star_p, star_n + 1));
                    p = star_p + 1;
                    n = star_n + 1;
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|character| *character == '*')
}

fn is_package_folder(name: &str) -> bool {
    PACKAGE_FOLDERS.iter().any(|folder| same_name(folder, name))
}

/// 大文字小文字を区別しないファイルシステム (Windows・macOS) では、TypeScript も区別しない。
const CASE_INSENSITIVE: bool = cfg!(any(windows, target_os = "macos"));

fn same_name(a: &str, b: &str) -> bool {
    if CASE_INSENSITIVE {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

fn same_char(a: char, b: char) -> bool {
    if CASE_INSENSITIVE {
        a.eq_ignore_ascii_case(&b)
    } else {
        a == b
    }
}

/// `extends` の先 (書いた順)。形が違えば None。
fn extended_names(raw: &Map<String, Value>) -> Option<Vec<String>> {
    match raw.get("extends") {
        None | Some(Value::Null) => Some(Vec::new()),
        Some(Value::String(parent)) => Some(vec![parent.clone()]),
        Some(Value::Array(parents)) => parents
            .iter()
            .map(|parent| parent.as_str().map(str::to_owned))
            .collect(),
        Some(_) => None,
    }
}

impl ConfigLoader {
    /// `canonical` の設定の `compilerOptions` の `key`。[`Self::strip_setting`] と同じく、設定自身の
    /// 値が最も優先し、配列の `extends` は後ろの設定が優先する (`null` も前の値を打ち消す)。読めない・
    /// 解けなければ None。
    fn option_setting(
        &mut self,
        canonical: &Path,
        key: &str,
        depth: usize,
        visiting: &mut Vec<PathBuf>,
    ) -> Option<OptionSetting> {
        if depth > MAX_CHAIN_DEPTH || visiting.iter().any(|path| path == canonical) {
            return None;
        }
        let raw = self.read(canonical)?;
        let directory = canonical.parent()?.to_path_buf();
        if let Some(options) = raw.get("compilerOptions") {
            match options.as_object()?.get(key) {
                None => {}
                Some(Value::Null) => return Some(OptionSetting::Reset),
                Some(value) => return Some(OptionSetting::Value(value.clone(), directory)),
            }
        }
        // `jsconfig.json` は `allowJs` を既定で有効にし (TypeScript の `getDefaultCompilerOptions`)、
        // その既定は継いだ設定の値より優先する。
        if key == "allowJs"
            && canonical
                .file_name()
                .is_some_and(|name| name == "jsconfig.json")
        {
            return Some(OptionSetting::Value(Value::Bool(true), directory));
        }
        let parents = extended_names(&raw)?;
        visiting.push(canonical.to_path_buf());
        let mut setting = Some(OptionSetting::Unset);
        for parent in parents.iter().rev() {
            let inherited = self
                .extended_path(&directory, parent, Path::new(""))
                .and_then(|path| self.option_setting(&path, key, depth + 1, visiting));
            if inherited != Some(OptionSetting::Unset) {
                setting = inherited;
                break;
            }
        }
        visiting.pop();
        setting
    }

    /// `canonical` の設定 (ディレクトリ `leaf`) の出力のディレクトリ (`outDir`・`declarationDir`)。
    /// 文字列でない値は TypeScript と同じく使わない。読めない・解けなければ None。
    fn output_directories(&mut self, canonical: &Path, leaf: &Path) -> Option<Vec<PathBuf>> {
        let mut outputs = Vec::new();
        for key in ["outDir", "declarationDir"] {
            if let OptionSetting::Value(Value::String(text), directory) =
                self.option_setting(canonical, key, 0, &mut Vec::new())?
            {
                outputs.push(normalize_lexically(&option_path(&text, &directory, leaf)?)?);
            }
        }
        Some(outputs)
    }

    /// `canonical` の設定の `stripInternal`。設定自身の値が最も優先し、配列の `extends` は
    /// 後ろの設定が優先する (`null` も前の値を打ち消す)。読めない・解けなければ None。
    fn strip_setting(
        &mut self,
        canonical: &Path,
        depth: usize,
        visiting: &mut Vec<PathBuf>,
    ) -> Option<Setting> {
        if depth > MAX_CHAIN_DEPTH || visiting.iter().any(|path| path == canonical) {
            return None;
        }
        let raw = self.read(canonical)?;
        if let Some(options) = raw.get("compilerOptions") {
            match options.as_object()?.get("stripInternal") {
                None => {}
                Some(Value::Bool(value)) => return Some(Setting::Value(*value)),
                Some(Value::Null) => return Some(Setting::Reset),
                Some(_) => return None,
            }
        }
        let directory = canonical.parent()?.to_path_buf();
        let parents = extended_names(&raw)?;
        visiting.push(canonical.to_path_buf());
        let mut setting = Some(Setting::Unset);
        for parent in parents.iter().rev() {
            // 解析するディレクトリの外の設定 (モノレポの共通の設定) も辿る。
            let inherited = self
                .extended_path(&directory, parent, Path::new(""))
                .and_then(|path| self.strip_setting(&path, depth + 1, visiting));
            if inherited != Some(Setting::Unset) {
                setting = inherited;
                break;
            }
        }
        visiting.pop();
        setting
    }

    /// `canonical` の設定の `files`・`include`・`exclude`。TypeScript と同じく、設定自身が書いた
    /// ものが優先し、書いていないものは `extends` の後ろの設定のものを継ぐ。読めない・解けなければ
    /// None。
    fn raw_inputs(
        &mut self,
        canonical: &Path,
        depth: usize,
        visiting: &mut Vec<PathBuf>,
    ) -> Option<RawInputs> {
        if depth > MAX_CHAIN_DEPTH || visiting.iter().any(|path| path == canonical) {
            return None;
        }
        let raw = self.read(canonical)?;
        let directory = canonical.parent()?.to_path_buf();
        let parents = extended_names(&raw)?;
        visiting.push(canonical.to_path_buf());
        let mut inputs = Some(RawInputs::default());
        for parent in &parents {
            let inherited = self
                .extended_path(&directory, parent, Path::new(""))
                .and_then(|path| self.raw_inputs(&path, depth + 1, visiting));
            match (inherited, inputs.as_mut()) {
                (Some(inherited), Some(inputs)) => {
                    if inherited.files.is_some() {
                        inputs.files = inherited.files;
                    }
                    if inherited.include.is_some() {
                        inputs.include = inherited.include;
                    }
                    if inherited.exclude.is_some() {
                        inputs.exclude = inherited.exclude;
                    }
                }
                _ => inputs = None,
            }
        }
        visiting.pop();
        let mut inputs = inputs?;
        for (key, slot) in [
            ("files", &mut inputs.files),
            ("include", &mut inputs.include),
            ("exclude", &mut inputs.exclude),
        ] {
            match raw.get(key) {
                None | Some(Value::Null) => {}
                Some(Value::Array(specs)) => {
                    *slot = Some(
                        specs
                            .iter()
                            .map(|spec| Some((spec.as_str()?.to_owned(), directory.clone())))
                            .collect::<Option<_>>()?,
                    );
                }
                Some(_) => return None,
            }
        }
        Some(inputs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(text: &str) -> Vec<String> {
        text.split('/').map(str::to_owned).collect()
    }

    #[test]
    fn include_patterns_follow_typescript_wildcards() {
        let target = pattern("r/src/lib.ts");
        assert!(pattern_matches(&pattern("r/src/**/*"), &target, false));
        assert!(pattern_matches(&pattern("r/**/*.ts"), &target, false));
        assert!(pattern_matches(&pattern("r/src/l?b.ts"), &target, false));
        assert!(!pattern_matches(&pattern("r/test/**/*"), &target, false));
        assert!(!pattern_matches(&pattern("r/**/*.js"), &target, false));
        // `**` と先頭の `*` は `.` で始まる名前とパッケージのディレクトリを辿らない
        assert!(!pattern_matches(
            &pattern("r/**/*"),
            &pattern("r/.storybook/lib.ts"),
            false
        ));
        assert!(!pattern_matches(
            &pattern("r/*/lib.ts"),
            &pattern("r/node_modules/lib.ts"),
            false
        ));
        assert!(pattern_matches(
            &pattern("r/.storybook/*"),
            &pattern("r/.storybook/lib.ts"),
            false
        ));
        // `exclude` はディレクトリの下にも当たり、`**` は何でも辿る
        assert!(pattern_matches(&pattern("r/src"), &target, true));
        assert!(pattern_matches(
            &pattern("r/**/*.ts"),
            &pattern("r/.cache/lib.ts"),
            true
        ));
    }

    #[test]
    fn wildcards_match_whole_names() {
        assert!(wildcard_matches("*.ts", "lib.ts", false));
        assert!(wildcard_matches("*.test.ts", "a.test.ts", false));
        assert!(!wildcard_matches("*.test.ts", "a.ts", false));
        assert!(wildcard_matches("l*b*.ts", "library.ts", false));
        assert!(!wildcard_matches("l?b.ts", "liib.ts", false));
        // `include` の最後の区切りの `*` は、末尾の `.min.js` の `.` に当たらない
        assert!(!wildcard_matches("*.js", "loader.min.js", true));
        assert!(!wildcard_matches("*", "loader.min.js", true));
        assert!(wildcard_matches("*.min.js", "loader.min.js", true));
        assert!(wildcard_matches("loader.*", "loader.min.js", true));
        assert!(wildcard_matches("*.js", "loader.min.js", false));
    }
}
