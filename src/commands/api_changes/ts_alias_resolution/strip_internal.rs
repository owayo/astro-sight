//! ファイルに当たる TypeScript の `stripInternal` の設定 (Issue #274)。
//!
//! ファイルから近いディレクトリの順に、tsconfig*.json / jsconfig*.json を見る。ファイルを入力に
//! 含む設定 (`files`・`include`・`exclude` の実効値。`extends` で継いだものも) のあるディレクトリで
//! 決め、そのどれかが実効で `stripInternal` を有効にすれば有効とする (宣言は `tsconfig.build.json`
//! のような別名の設定で出すことが多い)。ファイルを入力に含まない設定は、判定を止めず、数えも
//! しない。設定は入力から import で取り込んだファイルの宣言も出すが、TypeScript と同じ解決で
//! 取り込むと確かめられなければ、宣言ファイルに残るとみなす (消えるとみなすと、宣言ファイルに残る
//! 公開の宣言の削除を見逃す)。`"files": ["src/index.ts"]` のような入口だけを書いた出力の設定が
//! import で取り込むファイルには、その設定の `stripInternal` をまだ当てない。
//!
//! `stripInternal` の実効値は、設定自身の値、配列の `extends` の後ろの設定、前の設定の順に決まり、
//! `null` は既定 (無効) に戻す。読めない・解けない設定は、ファイルを含み得るものとして扱い、
//! `stripInternal` を有効にするとはみなさない。`extends` は解析するディレクトリの外も辿る。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::{
    ConfigLoader, MAX_ANCESTORS, MAX_CHAIN_DEPTH, is_config_name, missing, normalize_lexically,
    option_path, regular_file,
};

/// TypeScript がワイルドカードで辿らないディレクトリ。
const PACKAGE_FOLDERS: [&str; 3] = ["node_modules", "bower_components", "jspm_packages"];

pub(crate) struct StripInternalConfigs {
    root: Option<PathBuf>,
    loader: ConfigLoader,
    /// ディレクトリ → その設定 (一覧を読めなければ None)。
    directories: HashMap<PathBuf, Option<Vec<PathBuf>>>,
    /// 設定 → 入力 (読めない・解けなければ None)。
    inputs: HashMap<PathBuf, Option<Inputs>>,
    /// 設定 → `stripInternal` を実効で有効にするか。
    strips: HashMap<PathBuf, bool>,
}

impl StripInternalConfigs {
    pub(crate) fn new(dir: &str) -> Self {
        Self {
            root: std::path::absolute(Path::new(dir))
                .ok()
                .and_then(|path| path.canonicalize().ok()),
            loader: ConfigLoader::default(),
            directories: HashMap::new(),
            inputs: HashMap::new(),
            strips: HashMap::new(),
        }
    }

    /// `path` (`dir` 相対) のファイルの宣言を出し得る設定が、`stripInternal` を実効で有効に
    /// するか。削除したファイルのディレクトリが無くても、祖先の設定を見る。
    pub(crate) fn strips(&mut self, path: &str) -> bool {
        let Some(file) = self.root.as_ref().map(|root| root.join(path)) else {
            return false;
        };
        let Some(start) = file.parent() else {
            return false;
        };
        let target = components(&file);
        for (depth, directory) in start.ancestors().enumerate() {
            if depth > MAX_ANCESTORS {
                break;
            }
            let Some(configs) = self.directory_configs(directory) else {
                break;
            };
            let mut decided: Option<bool> = None;
            for config in configs {
                match self.membership(&config, &target) {
                    Membership::Includes | Membership::Unknown => {
                        let strips = self.config_strips(&config);
                        decided = Some(decided.unwrap_or(false) || strips);
                    }
                    Membership::Excludes => {}
                }
            }
            if let Some(strips) = decided {
                return strips;
            }
        }
        false
    }

    fn directory_configs(&mut self, directory: &Path) -> Option<Vec<PathBuf>> {
        self.directories
            .entry(directory.to_path_buf())
            .or_insert_with(|| read_directory_configs(directory))
            .clone()
    }

    fn membership(&mut self, config: &Path, target: &[String]) -> Membership {
        let loader = &mut self.loader;
        let inputs = self.inputs.entry(config.to_path_buf()).or_insert_with(|| {
            let raw = loader.raw_inputs(config, 0, &mut Vec::new())?;
            Inputs::resolve(raw, config.parent()?)
        });
        match inputs {
            Some(inputs) => inputs.membership(target),
            None => Membership::Unknown,
        }
    }

    fn config_strips(&mut self, config: &Path) -> bool {
        if let Some(&strips) = self.strips.get(config) {
            return strips;
        }
        let strips =
            self.loader.strip_setting(config, 0, &mut Vec::new()) == Some(Setting::Value(true));
        self.strips.insert(config.to_path_buf(), strips);
        strips
    }
}

/// `directory` の tsconfig*.json / jsconfig*.json (正規化したパス)。ディレクトリが無ければ空
/// (削除したファイルのディレクトリ)、一覧を読めなければ None。
fn read_directory_configs(directory: &Path) -> Option<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if missing(&error) => return Some(Vec::new()),
        Err(_) => return None,
    };
    let mut configs = Vec::new();
    for entry in entries {
        let entry = entry.ok()?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if is_config_name(name)
            && let Some(canonical) = regular_file(&entry.path())
        {
            configs.push(canonical);
        }
    }
    configs.sort();
    Some(configs)
}

/// 設定がファイルを入力に含むか。
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
    /// ディレクトリの `**/*`、`files` だけなら `include` は無い。`${configDir}` は入力を決める
    /// 設定 (`leaf`) のディレクトリで解く。
    fn resolve(raw: RawInputs, leaf: &Path) -> Option<Self> {
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
            None => Vec::new(),
        };
        Some(Self {
            files,
            include,
            exclude,
        })
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
    component_matches(first, name, exclude) && pattern_matches(rest, target_rest, exclude)
}

/// パターンの区切り 1 つが名前に当たるか。`include` では、ワイルドカードのある区切りはパッケージの
/// ディレクトリに当たらず、先頭の `*`・`?` は `.` で始まる名前の先頭に当たらない。
fn component_matches(pattern: &str, name: &str, exclude: bool) -> bool {
    if !pattern.contains(['*', '?']) {
        return same_name(pattern, name);
    }
    if !exclude {
        if is_package_folder(name) {
            return false;
        }
        if name.starts_with('.') {
            return match pattern.strip_prefix('*') {
                Some(rest) => wildcard_matches(rest, name),
                None => !pattern.starts_with('?') && wildcard_matches(pattern, name),
            };
        }
    }
    wildcard_matches(pattern, name)
}

/// `*` (任意の文字列) と `?` (任意の 1 文字) のパターンが、名前の全体に当たるか。
fn wildcard_matches(pattern: &str, name: &str) -> bool {
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
        assert!(wildcard_matches("*.ts", "lib.ts"));
        assert!(wildcard_matches("*.test.ts", "a.test.ts"));
        assert!(!wildcard_matches("*.test.ts", "a.ts"));
        assert!(wildcard_matches("l*b*.ts", "library.ts"));
        assert!(!wildcard_matches("l?b.ts", "liib.ts"));
    }
}
