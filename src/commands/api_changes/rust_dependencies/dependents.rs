//! 束縛の変化と、それを名指し得る差分に無いファイル (Issue #300)。
//!
//! 差分の中のファイルの旧新の束縛の指紋をスコープ (crate root からの経路) ごとに比べ、束縛の変わった
//! 名前と、中身 (glob・item 位置のマクロの呼び出し・`mod` の宣言) の変わった module を集める。そこから
//! 固定点で広げる: 変わった名前を識別子として含むか、変わった module を名指し得る package のファイルを
//! 開き、そのスコープの全ての名前を旧新の版で解いて比べ、変わった名前を足す (変えていないファイルの
//! 別名の右辺が、変わった名前を通すとき)。glob の先が変われば、増えた先・消えた先の名前を足す。旧新とも
//! 解けない束縛は、その束縛が変わった名前に触れるときだけ足す。当たったファイルのうち、宣言の契約の
//! パスが旧新で変わるものを比べ直す。
//!
//! 移した定義 (差分の中で、ある module から名前の定義が消え、別の module に同じ名前・種類・中身の
//! 定義が現れた) は、新しい版で移す前の場所で識別する。

use super::crates::{CrateVersion, Located, crate_package};
use super::resolver::absolute_place;
use super::test_scopes::TestScopes;
use super::*;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// 束縛の変わり得る名前と module。
#[derive(Debug, Default)]
pub(crate) struct ChangeSet {
    /// 束縛が変わった名前。
    names: HashSet<String>,
    /// 中身 (glob・マクロ・`mod` の宣言) が変わった module (crate root からの経路)。
    modules: BTreeSet<Vec<String>>,
    /// 列挙できない変化 (どの名前も変わり得る)。
    all: bool,
}

impl ChangeSet {
    /// 契約が名指すパス `segments` (宣言を囲む module の crate root からの経路 `module`) の束縛が、
    /// 旧新で変わり得るか。
    pub(crate) fn affects(&self, segments: &[String], module: &[String]) -> bool {
        self.all
            || self
                .modules
                .iter()
                .any(|changed| module.starts_with(changed))
            || segments.iter().any(|segment| {
                self.names.contains(segment)
                    || self
                        .modules
                        .iter()
                        .any(|changed| changed.last() == Some(segment))
            })
    }

    /// ファイル (テキスト `text`、module の経路 `module`) が、変わった束縛を名指し得るか。変わった
    /// 名前を識別子として書いたファイル (`names` は変わった名前を探す automaton)。中身の変わった module
    /// は、その配下のファイル (`super::` で名指す) か、その名前を書いたファイル (`a::changed::X`・
    /// 祖先の別名を経る `facade::changed::X`) が名指し得る。
    fn mentioned_by(
        &self,
        names: Option<&aho_corasick::AhoCorasick>,
        text: &[u8],
        module: Option<&[String]>,
    ) -> bool {
        self.all
            || names.is_some_and(|names| {
                names
                    .find_overlapping_iter(text)
                    .any(|found| word_at(text, found.start(), found.end()))
            })
            || self.modules.iter().any(|changed| {
                let Some(name) = changed.last() else {
                    // crate root はどのファイルも名指し得る。
                    return true;
                };
                module.is_some_and(|module| module.starts_with(changed)) || mentions(text, name)
            })
    }

    /// 変わった名前を探す automaton (名前が無ければ None)。
    fn names_automaton(&self) -> Option<aho_corasick::AhoCorasick> {
        if self.names.is_empty() {
            return None;
        }
        let mut names: Vec<&String> = self.names.iter().collect();
        names.sort();
        aho_corasick::AhoCorasick::new(names).ok()
    }
}

/// テキスト `text` の `start..end` が、識別子の単語の全体か (前後が識別子の文字でない)。
fn word_at(text: &[u8], start: usize, end: usize) -> bool {
    let is_identifier = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_' || byte >= 0x80;
    start
        .checked_sub(1)
        .is_none_or(|before| !is_identifier(text[before]))
        && text.get(end).is_none_or(|&after| !is_identifier(after))
}

/// テキスト `text` に、識別子 `name` が単語として現れるか。
fn mentions(text: &[u8], name: &str) -> bool {
    !name.is_empty()
        && memchr::memmem::find_iter(text, name.as_bytes())
            .any(|start| word_at(text, start, start + name.len()))
}

/// 移した定義: (移した先の module の経路, 名前) → 移す前の module の経路。
pub(crate) type MoveMap = HashMap<(Vec<String>, String), Vec<String>>;

/// package の束縛の変化と、比べ直すファイル。
pub(super) struct Analysis {
    pub(super) changes: ChangeSet,
    pub(super) moves: MoveMap,
    pub(super) dependents: Vec<(String, String)>,
    pub(super) incomplete: Option<String>,
    /// 2 つ以上の module に使うファイル (規約の場所と `#[path]`、2 つの `#[path]`)。どの module として
    /// 比べるか決められない。
    pub(super) multi_placed: HashSet<String>,
}

/// 名前を探すファイルの数・1 つの大きさ・読む量の上限。超えれば探し切れなかったとする。
const MAX_SCANNED_FILES: usize = 10_000;
const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;
const MAX_SCANNED_BYTES: usize = 64 * 1024 * 1024;

/// スコープ (crate root からの経路) の、旧新のファイルとファイルの中の経路。
type ScopeSite = (File, Vec<String>);

/// 版の、差分の中のファイルのスコープの指紋と場所。
#[derive(Default)]
struct VersionScopes {
    fingerprints: BTreeMap<Vec<String>, ScopeFingerprint>,
    sites: BTreeMap<Vec<String>, ScopeSite>,
}

impl VersionScopes {
    fn add(&mut self, file: &File) {
        let module = file.file_module().unwrap_or_default().to_vec();
        for (scope, fingerprint) in file.fingerprint() {
            let mut absolute = module.clone();
            absolute.extend(scope.iter().cloned());
            let entry = self.fingerprints.entry(absolute.clone()).or_default();
            for (name, entries) in fingerprint.names {
                entry.names.entry(name).or_default().extend(entries);
            }
            entry.globs.extend(fingerprint.globs);
            entry.macros.extend(fingerprint.macros);
            entry.modules.extend(fingerprint.modules);
            self.sites.entry(absolute).or_insert((file.clone(), scope));
        }
    }
}

/// 定義 (項目・型別名・定数) の指紋の項目か。
fn is_definition_entry(entry: &str) -> bool {
    ["item ", "alias ", "const "]
        .iter()
        .any(|prefix| entry.starts_with(prefix))
}

/// まだ比べていない束縛。変わった名前や module に触れたら旧新の版で解いて比べる (触れなければ、
/// 入力が変わらないので旧新で同じ)。
struct Deferred {
    old_file: Option<File>,
    new_file: Option<File>,
    /// ファイルの中のスコープ (inline module の経路)。
    scope: Vec<String>,
    /// 束縛の名前。None なら、スコープの glob。
    name: Option<String>,
    /// 束縛が名指すパス (旧新の両方)。
    paths: Vec<Vec<String>>,
    /// スコープの crate root からの経路。
    absolute: Vec<String>,
}

/// ファイル (旧 `old_file`・新 `new_file`) の全てのスコープの束縛と glob を `deferred` に足す。
/// テストの module のスコープは足さない。
fn defer_bindings(
    old_file: Option<File>,
    new_file: Option<File>,
    tests: &TestScopes<'_, '_>,
    deferred: &mut Vec<Deferred>,
) {
    let files: Vec<&File> = old_file.iter().chain(new_file.iter()).collect();
    let mut scopes: BTreeSet<Vec<String>> = BTreeSet::new();
    for file in &files {
        scopes.extend(file.scope_paths().cloned());
    }
    // スコープの crate root からの経路 (新しい版を優先)。
    let module = new_file
        .as_ref()
        .or(old_file.as_ref())
        .and_then(|file| file.file_module())
        .unwrap_or_default()
        .to_vec();
    for scope in scopes {
        let mut absolute = module.clone();
        absolute.extend(scope.iter().cloned());
        if tests.contains(&absolute) {
            continue;
        }
        let mut names: BTreeSet<String> = BTreeSet::new();
        for file in &files {
            names.extend(file.scope_names(&scope));
        }
        for name in names {
            let paths = files
                .iter()
                .flat_map(|file| file.binding_paths(&scope, &name))
                .collect();
            deferred.push(Deferred {
                old_file: old_file.clone(),
                new_file: new_file.clone(),
                scope: scope.clone(),
                name: Some(name),
                paths,
                absolute: absolute.clone(),
            });
        }
        let paths: Vec<Vec<String>> = files
            .iter()
            .flat_map(|file| file.glob_paths(&scope))
            .collect();
        if !paths.is_empty() {
            deferred.push(Deferred {
                old_file: old_file.clone(),
                new_file: new_file.clone(),
                scope,
                name: None,
                paths,
                absolute,
            });
        }
    }
}

/// 変わった名前や module に触れた束縛を旧新の版で解いて比べ、変わったか解けなければ変化に足す。触れる
/// ものが無くなるまで繰り返す。
fn settle(
    old: &CrateVersion<'_>,
    new: &CrateVersion<'_>,
    moves: &MoveMap,
    deferred: &mut Vec<Deferred>,
    changes: &mut ChangeSet,
) {
    loop {
        let (ready, rest): (Vec<Deferred>, Vec<Deferred>) =
            deferred.drain(..).partition(|binding| {
                changes.all
                    || changes
                        .modules
                        .iter()
                        .any(|changed| binding.absolute.starts_with(changed))
                    || binding
                        .paths
                        .iter()
                        .any(|path| changes.affects(path, &binding.absolute))
            });
        *deferred = rest;
        if ready.is_empty() {
            return;
        }
        for binding in &ready {
            compare_binding(old, new, moves, binding, changes);
        }
    }
}

/// 差分のファイル `files` (package `package` の lib crate のソースを含む) と、内容が同じ rename
/// `renames` (旧いパス, 新しいパス) の旧新の束縛の変化と、比べ直すファイル。
pub(super) fn analyze(
    dir: &str,
    package: &str,
    files: &[&crate::models::impact::DiffFile],
    renames: &[(String, String)],
    old: &CrateVersion<'_>,
    new: &CrateVersion<'_>,
) -> Analysis {
    let in_package = |path: &str| crate_package(dir, path).as_deref() == Some(package);
    let mut changes = ChangeSet::default();
    let mut incomplete = None;
    let mut old_scopes = VersionScopes::default();
    let mut new_scopes = VersionScopes::default();
    // 固定点で旧新を解き比べるファイル (旧, 新)。
    let mut queue: Vec<(Option<File>, Option<File>)> = Vec::new();
    // 差分の中のファイルの新しい版 (`#[path]` の探索の起点)。
    let mut diff_new_files: Vec<File> = Vec::new();
    let mut diff_paths: HashSet<&str> = HashSet::new();
    for df in files {
        diff_paths.insert(&df.old_path);
        diff_paths.insert(&df.new_path);
        let mut opened = [None, None];
        for (slot, (version, path, scopes)) in [
            (old, &df.old_path, &mut old_scopes),
            (new, &df.new_path, &mut new_scopes),
        ]
        .into_iter()
        .enumerate()
        {
            if !in_package(path) {
                continue;
            }
            match version.open_file(path) {
                Opened::File(file) => {
                    scopes.add(&file);
                    opened[slot] = Some(file);
                }
                // lib の crate の module の木に無いファイルは関係しない。module を決められなければ、
                // どの名前も変わり得る。
                _ => {
                    if version
                        .tree_modules(path)
                        .is_none_or(|modules| !modules.is_empty())
                    {
                        changes.all = true;
                    }
                }
            }
        }
        let [old_file, new_file] = opened;
        diff_new_files.extend(new_file.iter().cloned());
        if old_file.is_some() || new_file.is_some() {
            queue.push((old_file, new_file));
        }
    }
    // テストの module (`#[cfg(test)]`) の中身の変化は、公開の契約 (テストでない build) に関わらない。
    // `mod tests` の中の変化で、`tests` やその中の名前を書いた全てのファイルを探さない。
    let tests = TestScopes::new(old, new);
    collect_scope_changes(&old_scopes, &new_scopes, &tests, &mut changes);
    let moves = collect_moves(old, new, &old_scopes, &new_scopes, &changes);

    // 変わった束縛を名指し得るファイルを、固定点で探す。
    let mut package_files: Vec<String> = match package_sources(dir, package) {
        Some(paths) => paths
            .into_iter()
            .filter(|path| !diff_paths.contains(path.as_str()))
            .collect(),
        None => {
            incomplete = Some("the package's Rust sources could not be listed".to_owned());
            Vec::new()
        }
    };
    // ファイルのテキスト (大きすぎて読めなければ None)。
    let mut texts: HashMap<String, Option<Vec<u8>>> = HashMap::new();
    let mut scanned_bytes = 0usize;
    let mut path_files_added = false;
    // `#[path]` でたどった、規約の経路と違う module に置いたファイルの実際の module。
    let mut actual_modules: HashMap<String, Vec<String>> = HashMap::new();
    // `#[path]` の探索でたどったファイルの module (新しい版)。
    let mut placements: HashMap<String, BTreeSet<Vec<String>>> = HashMap::new();
    let mut hits: BTreeSet<String> = BTreeSet::new();
    let mut deferred: Vec<Deferred> = Vec::new();
    loop {
        while let Some((old_file, new_file)) = queue.pop() {
            defer_bindings(old_file, new_file, &tests, &mut deferred);
        }
        settle(old, new, &moves, &mut deferred, &mut changes);
        let names = changes.names_automaton();
        let mut found = false;
        for path in &package_files {
            if hits.contains(path) {
                continue;
            }
            if !texts.contains_key(path) {
                if texts.len() >= MAX_SCANNED_FILES {
                    incomplete = Some(format!(
                        "more than {MAX_SCANNED_FILES} Rust sources to search"
                    ));
                    break;
                }
                let text = new.read(path).filter(|text| text.len() <= MAX_FILE_BYTES);
                scanned_bytes += text.as_ref().map_or(0, Vec::len);
                if scanned_bytes > MAX_SCANNED_BYTES {
                    incomplete = Some(format!(
                        "more than {MAX_SCANNED_BYTES} bytes of Rust sources to search"
                    ));
                    break;
                }
                texts.insert(path.clone(), text);
            }
            let module = actual_modules
                .get(path)
                .cloned()
                .or_else(|| source_module_path(path).map(|(_, module)| module));
            let mentioned = match &texts[path] {
                Some(text) => changes.mentioned_by(names.as_ref(), text, module.as_deref()),
                // 読めない (大きすぎる) ファイルは、名指し得るとして比べ直す。
                None => true,
            };
            if !mentioned {
                continue;
            }
            hits.insert(path.clone());
            found = true;
            let old_file = match old.open_file(path) {
                Opened::File(file) => Some(file),
                _ => None,
            };
            let new_file = match new.open_file(path) {
                Opened::File(file) => Some(file),
                _ => None,
            };
            if old_file.is_none() || new_file.is_none() {
                // module を決められない・構文エラー。lib の crate の外と分かれば関係しない。
                if new
                    .tree_modules(path)
                    .is_none_or(|modules| !modules.is_empty())
                {
                    incomplete.get_or_insert_with(|| {
                        format!("the module of {path} could not be determined or parsed")
                    });
                }
                continue;
            }
            queue.push((old_file, new_file));
        }
        if incomplete.is_some() {
            break;
        }
        // `#[path]` を書いた crate では、`src/` の外のファイルも module になり得る。crate root から
        // `mod` 宣言をたどった木のファイルも探す。
        // `#[path]` で `src/` の外のファイルを module にした crate では、そのファイルも探す。`#[path]` を
        // 書いたファイル (文字列やコメントの中の字面で決めないよう、宣言を解いて確かめる) の `mod` の
        // 先を、`src/` の外へたどる。
        if !path_files_added {
            path_files_added = true;
            let known: HashSet<String> = package_files.iter().cloned().collect();
            // 起点: `#[path]` を書いた package のファイルと、差分の中のファイル。
            let mut pending: Vec<File> = texts
                .iter()
                .filter(|(path, text)| {
                    text.as_deref().is_some_and(|text| {
                        has_path_attribute(text)
                            && DependencyIndex::new(text, path)
                                .is_none_or(|index| index.declares_path_modules())
                    })
                })
                .filter_map(|(path, _)| match new.open_file(path) {
                    Opened::File(file) => Some(file),
                    _ => None,
                })
                .chain(
                    diff_new_files
                        .iter()
                        .filter(|file| file.declares_path_modules())
                        .cloned(),
                )
                .collect();
            let mut visited: HashSet<String> =
                pending.iter().map(|file| file.path().to_owned()).collect();
            let mut extra: BTreeSet<String> = BTreeSet::new();
            'walk: while let Some(file) = pending.pop() {
                let scopes: Vec<Vec<String>> = file.scope_paths().cloned().collect();
                for scope in scopes {
                    for name in file.scope_names(&scope) {
                        for declaration in file.module_declarations(&scope, &name) {
                            if declaration.inline {
                                continue;
                            }
                            // 開く前に、探す量の上限を確かめる。
                            if texts.len() + visited.len() >= MAX_SCANNED_FILES {
                                incomplete = Some(format!(
                                    "more than {MAX_SCANNED_FILES} Rust sources to search"
                                ));
                                break 'walk;
                            }
                            let Opened::File(child) = new.child(&file, &scope, &name, declaration)
                            else {
                                continue;
                            };
                            if !visited.insert(child.path().to_owned()) {
                                continue;
                            }
                            scanned_bytes += child.source.len();
                            if scanned_bytes > MAX_SCANNED_BYTES {
                                incomplete = Some(format!(
                                    "more than {MAX_SCANNED_BYTES} bytes of Rust sources to search"
                                ));
                                break 'walk;
                            }
                            // 規約の経路と違う module に置いたファイル (`#[path]` で開いたファイルと、その
                            // 中の `mod`) は、実際の module で名指し得るかを決め、その中もたどる。
                            let conventional =
                                source_module_path(child.path()).map(|(_, module)| module);
                            let actual = child.file_module().map(<[String]>::to_vec);
                            if let Some(actual) = &actual {
                                placements
                                    .entry(child.path().to_owned())
                                    .or_default()
                                    .insert(actual.clone());
                            }
                            let moved = actual.is_some() && actual != conventional;
                            if let (true, Some(actual)) = (moved, actual) {
                                actual_modules.insert(child.path().to_owned(), actual);
                            }
                            let outside =
                                !known.contains(child.path()) && !diff_paths.contains(child.path());
                            if outside {
                                extra.insert(child.path().to_owned());
                            }
                            if outside || moved {
                                pending.push(child);
                            }
                        }
                    }
                }
            }
            // 足したファイルと、実際の module が分かったファイルを、もう一度探す。
            found |= !extra.is_empty() || !actual_modules.is_empty();
            package_files.extend(extra);
            if incomplete.is_some() {
                break;
            }
        }
        if !found && queue.is_empty() {
            break;
        }
    }
    // 2 つ以上の module に使うファイル (`#[path]` でたどった module と、規約の場所の module)。
    let multi_placed: HashSet<String> = placements
        .into_iter()
        .filter_map(|(path, mut modules)| {
            if let Some((_, conventional)) = source_module_path(&path)
                && let Located::Found(file, scope) = new.locate_scope(&conventional)
                && scope.is_empty()
                && file.path() == path
            {
                modules.insert(conventional);
            }
            (modules.len() > 1).then_some(path)
        })
        .collect();
    // 契約のパスが旧新で変わるファイルだけを比べ直す (変わった名前を書いていても、その束縛が変わらない
    // ファイルは比べ直さない)。2 つ以上の module に使うファイルは、どの module として解くか決められない
    // ので、契約が変わった名前に触れれば比べ直す (触れるパスを unresolved にする)。
    let mut dependents: Vec<(String, String)> = hits
        .into_iter()
        .filter(|path| match (old.open_file(path), new.open_file(path)) {
            (_, Opened::File(new_file)) if multi_placed.contains(path) => {
                contracts_touch(&new_file, &changes)
            }
            (Opened::File(old_file), Opened::File(new_file)) => {
                contracts_changed(old, new, &moves, &old_file, &new_file, &changes)
            }
            _ => false,
        })
        .map(|path| (path.clone(), path))
        .collect();
    // 内容が同じ rename は、比べるファイルとしては扱われないので、契約が変わるなら比べ直す。
    for (old_path, new_path) in renames {
        match (old.open_file(old_path), new.open_file(new_path)) {
            (Opened::File(old_file), Opened::File(new_file)) => {
                if contracts_changed(old, new, &moves, &old_file, &new_file, &changes) {
                    dependents.push((old_path.clone(), new_path.clone()));
                }
            }
            _ => {
                if old
                    .tree_modules(old_path)
                    .is_none_or(|modules| !modules.is_empty())
                    || new
                        .tree_modules(new_path)
                        .is_none_or(|modules| !modules.is_empty())
                {
                    incomplete.get_or_insert_with(|| {
                        format!("the module of {new_path} could not be determined or parsed")
                    });
                }
            }
        }
    }
    Analysis {
        changes,
        moves,
        dependents,
        incomplete,
        multi_placed,
    }
}

/// スコープごとの指紋を旧新で比べ、束縛の変わった名前と、中身の変わった module を集める。テストの
/// module のスコープは比べない。
fn collect_scope_changes(
    old: &VersionScopes,
    new: &VersionScopes,
    tests: &TestScopes<'_, '_>,
    changes: &mut ChangeSet,
) {
    let empty = ScopeFingerprint::default();
    let scopes: BTreeSet<&Vec<String>> = old
        .fingerprints
        .keys()
        .chain(new.fingerprints.keys())
        .collect();
    for scope in scopes {
        if tests.contains(scope) {
            continue;
        }
        let before = old.fingerprints.get(scope).unwrap_or(&empty);
        let after = new.fingerprints.get(scope).unwrap_or(&empty);
        // テストの module の宣言 (`#[cfg(test)] mod tests;` を足す・変える) は、名前も module も
        // 変化に入れない。
        let test_declaration = |name: &String| {
            let mut module = scope.clone();
            module.push(name.clone());
            let only_module = |fingerprint: &ScopeFingerprint| {
                fingerprint
                    .names
                    .get(name)
                    .is_none_or(|entries| entries.iter().all(|entry| entry.starts_with("mod ")))
            };
            only_module(before) && only_module(after) && tests.contains(&module)
        };
        let names: BTreeSet<&String> = before.names.keys().chain(after.names.keys()).collect();
        for name in names {
            if before.names.get(name) != after.names.get(name) && !test_declaration(name) {
                changes.names.insert(name.clone());
            }
        }
        if before.globs != after.globs || before.macros != after.macros {
            changes.modules.insert(scope.clone());
        }
        for name in before.modules.iter().chain(&after.modules) {
            if before.names.get(name) != after.names.get(name) && !test_declaration(name) {
                let mut module = scope.clone();
                module.push(name.clone());
                changes.modules.insert(module);
            }
        }
    }
}

/// 移した定義。束縛の変わった名前ごとに、定義が消えた module と現れた module がそれぞれ 1 つで、
/// 名前・種類・中身 (型別名と定数は右辺を解いた並び) が同じもの。移した先に旧い版で、移す前に新しい
/// 版で、その名前が無いことを、差分に無いファイルも含めて確かめる。項目を先に求め、型別名と定数の
/// 右辺は項目の移動を当てて比べる。
fn collect_moves(
    old: &CrateVersion<'_>,
    new: &CrateVersion<'_>,
    old_scopes: &VersionScopes,
    new_scopes: &VersionScopes,
    changes: &ChangeSet,
) -> MoveMap {
    let mut moves = MoveMap::new();
    let definitions = |scopes: &VersionScopes, scope: &Vec<String>, name: &String| {
        scopes
            .fingerprints
            .get(scope)
            .and_then(|fingerprint| fingerprint.names.get(name))
            .is_some_and(|entries| entries.iter().any(|entry| is_definition_entry(entry)))
    };
    let present = |scopes: &VersionScopes, scope: &Vec<String>, name: &String| {
        scopes
            .fingerprints
            .get(scope)
            .is_some_and(|fingerprint| fingerprint.names.contains_key(name))
    };
    // その版の実際の module に、その名前の束縛が無いか (module ごと無いときも)。決められなければ false。
    let absent = |version: &CrateVersion<'_>, module: &[String], name: &str| match version
        .locate_scope(module)
    {
        Located::Found(file, scope) => !file.scope_names(&scope).iter().any(|known| known == name),
        Located::Absent => true,
        Located::Unknown => false,
    };
    let mut candidates = Vec::new();
    let mut names: Vec<&String> = changes.names.iter().collect();
    names.sort();
    for name in names {
        let removed: Vec<&Vec<String>> = old_scopes
            .fingerprints
            .keys()
            .filter(|scope| {
                definitions(old_scopes, scope, name) && !present(new_scopes, scope, name)
            })
            .collect();
        let added: Vec<&Vec<String>> = new_scopes
            .fingerprints
            .keys()
            .filter(|scope| {
                definitions(new_scopes, scope, name) && !present(old_scopes, scope, name)
            })
            .collect();
        if let ([from], [to]) = (removed.as_slice(), added.as_slice())
            && absent(old, to, name)
            && absent(new, from, name)
        {
            candidates.push(((*from).clone(), (*to).clone(), name.clone()));
        }
    }
    // 項目 (中身は種類と条件) を先に、型別名・定数は項目の移動を当てて右辺を比べる。
    for pass in 0..2 {
        for (from, to, name) in &candidates {
            if moves.contains_key(&(to.clone(), name.clone())) {
                continue;
            }
            let (Some((old_file, old_scope)), Some((new_file, new_scope))) =
                (old_scopes.sites.get(from), new_scopes.sites.get(to))
            else {
                continue;
            };
            let old_resolver = Resolver::new(Some(old), old_file.clone(), None, None);
            let new_resolver = Resolver::new(Some(new), new_file.clone(), Some(&moves), None);
            let (Some(before), Some(after)) = (
                old_resolver.sole_definition(old_file, old_scope, name),
                new_resolver.sole_definition(new_file, new_scope, name),
            ) else {
                continue;
            };
            let is_item = before.starts_with("item:");
            if (pass == 0) != is_item {
                continue;
            }
            if definition_content(&before).is_some()
                && definition_content(&before) == definition_content(&after)
            {
                moves.insert((to.clone(), name.clone()), from.clone());
            }
        }
    }
    moves
}

/// 束縛 `binding` を旧新の版で解いて比べ、変わったか解けなければ変化に足す。中身の変わった module を
/// 指す名前 (module の別名) も足す。glob の先が変われば、増えた先・消えた先の名前を足し、解けなければ
/// どの名前も変わり得るとする。
fn compare_binding(
    old: &CrateVersion<'_>,
    new: &CrateVersion<'_>,
    moves: &MoveMap,
    binding: &Deferred,
    changes: &mut ChangeSet,
) {
    let old_resolver = binding
        .old_file
        .as_ref()
        .map(|file| Resolver::new(Some(old), file.clone(), None, None));
    let new_resolver = binding
        .new_file
        .as_ref()
        .map(|file| Resolver::new(Some(new), file.clone(), Some(moves), None));
    let scope = &binding.scope;
    let Some(name) = &binding.name else {
        match (
            glob_targets(old_resolver.as_ref(), scope),
            glob_targets(new_resolver.as_ref(), scope),
        ) {
            (Some(before), Some(after)) => {
                // 変わらない先の名前は、その先の束縛の変化として伝わる。
                let added = after.iter().filter(|(key, _)| !before.contains_key(*key));
                let removed = before.iter().filter(|(key, _)| !after.contains_key(*key));
                for (_, module) in added.chain(removed) {
                    if let Some(module) = module {
                        changes.names.extend(module.file.scope_names(&module.scope));
                    }
                }
            }
            _ => changes.all = true,
        }
        return;
    };
    let resolve = |resolver: Option<&Resolver<'_>>| match resolver {
        Some(resolver) => resolver.resolve_binding(&resolver.current, scope, name),
        None => (Some(String::new()), Vec::new()),
    };
    let (before, before_modules) = resolve(old_resolver.as_ref());
    let (after, after_modules) = resolve(new_resolver.as_ref());
    // 中身の変わった module を指す名前 (module の別名) は、その先の名前が変わり得る。module 自身の
    // `mod` の宣言は、名前と親の module の名前で探す ([`ChangeSet::mentioned_by`]) ので足さない。
    let mut own = binding.absolute.clone();
    own.push(name.clone());
    let names_changed_module = before_modules.iter().chain(&after_modules).any(|target| {
        let module = module_path(target);
        module != own
            && changes
                .modules
                .iter()
                .any(|changed| module.starts_with(changed))
    });
    if before.is_none() || before != after || names_changed_module {
        changes.names.insert(name.clone());
    }
}

/// module のスコープの crate root からの経路。
fn module_path(target: &resolver::ModuleRef) -> Vec<String> {
    let mut module = target.file.file_module().unwrap_or_default().to_vec();
    module.extend(target.scope.iter().cloned());
    module
}

/// スコープ `scope` の glob の先 (識別の字面 → module のスコープ)。文脈が無ければ空。解けなければ None。
fn glob_targets(
    resolver: Option<&Resolver<'_>>,
    scope: &[String],
) -> Option<BTreeMap<String, Option<resolver::ModuleRef>>> {
    let Some(resolver) = resolver else {
        return Some(BTreeMap::new());
    };
    if resolver
        .current
        .scopes
        .get(scope)
        .is_none_or(|bindings| bindings.globs.is_empty())
    {
        return Some(BTreeMap::new());
    }
    let mut found = BTreeMap::new();
    for target in resolver.glob_targets(&resolver.current, scope)? {
        match target {
            resolver::Target::Module(module) => {
                found.insert(absolute_place(module_path(&module).iter()), Some(module));
            }
            resolver::Target::Identity(identity) => {
                found.insert(identity, None);
            }
        }
    }
    Some(found)
}

/// ファイル (旧 `old_file`・新 `new_file`。中身は同じ) の宣言の契約のうち、変わった名前や module に
/// 触れるパスを旧新の版で解き、束縛が変わるか解けないものがあるか。
fn contracts_changed(
    old: &CrateVersion<'_>,
    new: &CrateVersion<'_>,
    moves: &MoveMap,
    old_file: &File,
    new_file: &File,
    changes: &ChangeSet,
) -> bool {
    let old_resolver = Resolver::new(Some(old), old_file.clone(), None, None);
    let new_resolver = Resolver::new(Some(new), new_file.clone(), Some(moves), None);
    let module = new_file.file_module().unwrap_or_default().to_vec();
    let changed = |item: tree_sitter::Node<'_>, scope: &[String]| {
        let mut absolute = module.clone();
        absolute.extend(scope.iter().cloned());
        contract_references(item, &new_file.source)
            .iter()
            .filter(|reference| changes.affects(&reference.segments, &absolute))
            .any(|reference| {
                let before = old_resolver.resolve_path(
                    &resolver::ModuleRef {
                        file: old_file.clone(),
                        scope: scope.to_vec(),
                    },
                    reference,
                    0,
                );
                let after = new_resolver.resolve_path(
                    &resolver::ModuleRef {
                        file: new_file.clone(),
                        scope: scope.to_vec(),
                    },
                    reference,
                    0,
                );
                before.is_none() || before != after
            })
    };
    // module のスコープ (ファイルと inline module の中身) と、impl・trait・extern ブロックの中身を
    // たどる。関数の本体はたどらない。
    let mut pending = vec![(new_file.tree.root_node(), Vec::<String>::new())];
    while let Some((node, scope)) = pending.pop() {
        let mut cursor = node.walk();
        for item in node.named_children(&mut cursor) {
            match item.kind() {
                "mod_item" => {
                    if let (Some(body), Some(name)) = (
                        item.child_by_field_name("body"),
                        item.child_by_field_name("name")
                            .and_then(|name| name.utf8_text(&new_file.source).ok()),
                    ) {
                        let mut inner = scope.clone();
                        inner.push(unraw(name).into_owned());
                        pending.push((body, inner));
                    }
                }
                "impl_item" | "trait_item" | "foreign_mod_item" => {
                    if item.kind() == "trait_item" && changed(item, &scope) {
                        return true;
                    }
                    if let Some(body) = item.child_by_field_name("body") {
                        pending.push((body, scope.clone()));
                    }
                }
                "function_item"
                | "function_signature_item"
                | "struct_item"
                | "enum_item"
                | "union_item"
                | "type_item"
                | "const_item"
                | "static_item"
                    if changed(item, &scope) =>
                {
                    return true;
                }
                _ => {}
            }
        }
    }
    false
}

/// ファイル `file` の宣言の契約のパスが、変わった名前や module に触れるか (解かずに字面で)。
fn contracts_touch(file: &File, changes: &ChangeSet) -> bool {
    let module = file.file_module().unwrap_or_default().to_vec();
    let mut pending = vec![(file.tree.root_node(), Vec::<String>::new())];
    while let Some((node, scope)) = pending.pop() {
        let mut absolute = module.clone();
        absolute.extend(scope.iter().cloned());
        let mut cursor = node.walk();
        for item in node.named_children(&mut cursor) {
            match item.kind() {
                "mod_item" => {
                    if let (Some(body), Some(name)) = (
                        item.child_by_field_name("body"),
                        item.child_by_field_name("name")
                            .and_then(|name| name.utf8_text(&file.source).ok()),
                    ) {
                        let mut inner = scope.clone();
                        inner.push(unraw(name).into_owned());
                        pending.push((body, inner));
                    }
                }
                "impl_item" | "trait_item" | "foreign_mod_item" => {
                    if item.kind() == "trait_item"
                        && contract_references(item, &file.source)
                            .iter()
                            .any(|reference| changes.affects(&reference.segments, &absolute))
                    {
                        return true;
                    }
                    if let Some(body) = item.child_by_field_name("body") {
                        pending.push((body, scope.clone()));
                    }
                }
                "function_item"
                | "function_signature_item"
                | "struct_item"
                | "enum_item"
                | "union_item"
                | "type_item"
                | "const_item"
                | "static_item"
                    if contract_references(item, &file.source)
                        .iter()
                        .any(|reference| changes.affects(&reference.segments, &absolute)) =>
                {
                    return true;
                }
                _ => {}
            }
        }
    }
    false
}

/// テキストが `#[path = "…"]` か、`cfg_attr(…, path = …)` を書いていそうか (`path` の単語の前が
/// `[`・`(`・`,` で、後ろが `=`)。
fn has_path_attribute(text: &[u8]) -> bool {
    memchr::memmem::find_iter(text, b"path").any(|start| {
        let end = start + 4;
        if !word_at(text, start, end) {
            return false;
        }
        let before = text[..start]
            .iter()
            .rev()
            .find(|byte| !byte.is_ascii_whitespace());
        let after = text[end..].iter().find(|byte| !byte.is_ascii_whitespace());
        matches!(before, Some(b'[' | b'(' | b',')) && after == Some(&b'=')
    })
}

/// package `package` の lib の `src/` の下の Rust のソース (作業ツリー。`src/bin/` を除く)。列挙できな
/// ければ None。
fn package_sources(dir: &str, package: &str) -> Option<Vec<String>> {
    let src = if package.is_empty() {
        std::path::PathBuf::from("src")
    } else {
        std::path::Path::new(package).join("src")
    };
    let files = super::super::rust_public::collect_rust_rs_files(
        super::super::rust_public::RustSourceTree::Worktree,
        dir,
        &src,
    )?;
    let mut paths: Vec<String> = files
        .iter()
        .filter_map(|path| {
            let parts: Option<Vec<&str>> = path
                .components()
                .map(|part| part.as_os_str().to_str())
                .collect();
            Some(parts?.join("/"))
        })
        .filter(|path| crate_package(dir, path).as_deref() == Some(package))
        .collect();
    paths.sort();
    Some(paths)
}
