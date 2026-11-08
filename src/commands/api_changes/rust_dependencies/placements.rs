//! 版の lib crate の、ファイルの module の置き場と、その条件 (Issue #305)。
//!
//! crate root から `mod` の宣言をたどり、ファイルごとの置き場 (module の経路) と、その置き場に着く
//! 条件 (経路の `mod` の宣言の有る条件の AND。別の宣言から着けば OR) を集める。置き場の条件には、
//! 経路の宣言自身と囲む inline module の `cfg` と `cfg` に絞った `cfg_attr`、途中のファイルの先頭の
//! `#![cfg]` を入れ、着いたファイル自身の先頭の `#![cfg]` は入れない (項目の有る条件に入っている)。
//! どの設定でも成り立たない節 (`cfg(any())` など) は捨て、節の残らない宣言の先のファイルは開かず、
//! その module (と配下) が宣言されていることだけを残す。場所を決められない宣言
//! (`cfg_attr` の中の `path`、`#[path]` の付いた inline module の中の `mod x;`、既定の場所に両方ある・
//! どちらも無い) は、場所を決める入力とともに「不明」として残す。

use super::crates::CrateVersion;
use super::*;
use std::collections::{BTreeMap, BTreeSet};

/// たどる置き場の上限。
const MAX_PLACEMENTS: usize = 10_000;
/// たどるときに新しく読むソースの量の上限 (1 つ, 合計)。
const MAX_WALK_FILE_BYTES: usize = 2 * 1024 * 1024;
const MAX_WALK_BYTES: usize = 64 * 1024 * 1024;

/// たどる間だけ版の crate に読む量の上限を置く。
struct Limited<'v, 'a>(&'v CrateVersion<'a>);

impl Drop for Limited<'_, '_> {
    fn drop(&mut self) {
        self.0.set_limit(None);
    }
}
/// 置き場ごとの条件の節の上限。
const MAX_CLAUSES: usize = 64;

/// 置き場に着く条件の節 (属性の AND)。
#[derive(Clone, Debug)]
pub(crate) struct Clause {
    /// 経路の宣言の有る条件の属性 (外側から)。読めない属性があれば None。
    pub(crate) attributes: Option<Vec<AvailabilityAttr>>,
    /// 属性の生のトークン (宣言ごとに `|` で区切る)。旧新の同一性と、読めない属性のときの比較に使う。
    pub(crate) raw: Vec<String>,
    /// 経路の `mod` の宣言と、それを囲む inline module が全て `pub` (制限なし) か。
    pub(crate) public: bool,
}

impl Clause {
    /// crate root の節 (真)。
    fn root() -> Self {
        Self {
            attributes: Some(Vec::new()),
            raw: Vec::new(),
            public: true,
        }
    }

    /// この節に `mod` の宣言 `declaration` を続けた節。
    fn then(&self, declaration: &ModuleDeclaration) -> Self {
        let mut raw = self.raw.clone();
        raw.push("|".to_owned());
        raw.extend(declaration.availability_raw().iter().cloned());
        Self {
            attributes: self
                .attributes
                .as_ref()
                .zip(declaration.availability())
                .map(|(known, added)| known.iter().chain(added).cloned().collect()),
            raw,
            public: self.public && declaration.reachable(),
        }
    }

    /// どの設定でも成り立たないと確かめられるか (読めない属性か、原子が多すぎれば false)。
    fn unsatisfiable(&self) -> bool {
        self.attributes
            .as_deref()
            .is_some_and(super::super::rust_availability::unsatisfiable)
    }

    /// 旧新の比較の鍵 (生のトークンと公開か)。
    pub(crate) fn identity(&self) -> (Vec<String>, bool) {
        (self.raw.clone(), self.public)
    }
}

/// ファイルの module の置き場。
#[derive(Clone)]
pub(crate) struct Placement {
    /// ファイルのパス (`dir` 相対)。
    pub(crate) file: String,
    /// 置き場に着く条件 (OR)。
    pub(crate) clauses: Vec<Clause>,
    /// ファイルの索引。構文エラーか読めなければ None (中の項目と子の module を決められない)。
    pub(crate) index: Option<File>,
}

/// 場所を決められない `mod` の宣言。
#[derive(Clone)]
pub(crate) struct UnknownPlacement {
    /// 場所を決める入力の識別 (宣言したファイル、スコープ、名前、宣言と囲む inline module の属性)。
    pub(crate) identity: String,
    /// 宣言に着く条件 (OR)。
    pub(crate) clauses: Vec<Clause>,
}

/// 版の lib crate の、ファイルの module の置き場。
#[derive(Default)]
pub(crate) struct Placements {
    /// module の経路 → その module の中身のファイル (cfg で分けた別々のファイルも)。
    pub(crate) files: BTreeMap<Vec<String>, Vec<Placement>>,
    /// module の経路 → 場所を決められない宣言。
    pub(crate) unknown: BTreeMap<Vec<String>, Vec<UnknownPlacement>>,
    /// 置き場のファイルの中の inline module の経路 (crate root から)。
    pub(crate) inline: BTreeSet<Vec<String>>,
    /// どの設定でも着かない宣言の module の経路 (ファイルは開かない。配下の module もあるとする)。
    pub(crate) unreachable: BTreeSet<Vec<String>>,
    /// crate root を開けない・上限を超えた (たどり切れていない理由)。
    pub(crate) incomplete: Option<String>,
}

impl Placements {
    /// module の経路 `module` が、この版にあるか (ファイルの置き場か inline module か、場所を決め
    /// られない宣言)。
    pub(crate) fn has_module(&self, module: &[String]) -> bool {
        self.files.contains_key(module)
            || self.inline.contains(module)
            || self.unknown.contains_key(module)
            || (0..=module.len()).any(|depth| self.unreachable.contains(&module[..depth]))
    }
}

/// たどる途中の置き場。
struct Pending {
    clauses: Vec<Clause>,
    index: Option<File>,
}

/// 版 `version` の lib crate の置き場を、crate root から `mod` の宣言でたどって集める。
pub(crate) fn walk(version: &CrateVersion<'_>) -> Placements {
    version.set_limit(Some((MAX_WALK_FILE_BYTES, MAX_WALK_BYTES)));
    let _limited = Limited(version);
    let mut placements = Placements::default();
    let root = match version.root() {
        Opened::File(root) => root,
        _ => {
            placements.incomplete = Some("the crate root could not be read or parsed".to_owned());
            return placements;
        }
    };
    // module の経路の長さの順に処理する (親は子より短いので、子を処理するときには全ての親からの節が
    // 揃っている)。
    type Key = (usize, Vec<String>, String);
    let mut pending: BTreeMap<Key, Pending> = BTreeMap::new();
    pending.insert(
        (0, Vec::new(), root.path().to_owned()),
        Pending {
            clauses: vec![Clause::root()],
            index: Some(root),
        },
    );
    let mut processed = 0usize;
    // 先に開いたファイル (crate root など) も、大きさの上限を超えればたどり切れていないことにする。
    let oversized = |file: &File| file.source_len() > MAX_WALK_FILE_BYTES;
    while let Some(((_, module, path), current)) = pending.pop_first() {
        if current.index.as_ref().is_some_and(oversized) {
            placements.incomplete = Some(format!(
                "module file `{path}` exceeds {} MiB",
                MAX_WALK_FILE_BYTES / 1024 / 1024
            ));
            return placements;
        }
        processed += 1;
        if processed > MAX_PLACEMENTS {
            placements.incomplete = Some(format!(
                "more than {MAX_PLACEMENTS} module files are declared"
            ));
            return placements;
        }
        if current.clauses.len() > MAX_CLAUSES {
            placements.incomplete = Some(format!(
                "module `{}` is reached through more than {MAX_CLAUSES} conditional declarations",
                module.join("::")
            ));
            return placements;
        }
        placements
            .files
            .entry(module.clone())
            .or_default()
            .push(Placement {
                file: path.clone(),
                clauses: current.clauses.clone(),
                index: current.index.clone(),
            });
        let Some(file) = current.index else {
            continue;
        };
        let mut scopes: Vec<Vec<String>> = file.scope_paths().cloned().collect();
        scopes.sort();
        for scope in &scopes {
            if !scope.is_empty() {
                let mut inline = module.clone();
                inline.extend(scope.iter().cloned());
                placements.inline.insert(inline);
            }
        }
        for scope in scopes {
            let mut names = file.scope_names(&scope);
            names.sort();
            for name in names {
                for declaration in file.module_declarations(&scope, &name) {
                    if declaration.inline() {
                        continue;
                    }
                    // どの設定でも成り立たない節は捨てる (着く節の無い置き場も、宣言があることは残す)。
                    let clauses: Vec<Clause> = current
                        .clauses
                        .iter()
                        .map(|clause| clause.then(declaration))
                        .filter(|clause| !clause.unsatisfiable())
                        .collect();
                    let mut child_module = module.clone();
                    child_module.extend(scope.iter().cloned());
                    child_module.push(name.clone());
                    if clauses.is_empty() {
                        placements.unreachable.insert(child_module);
                        continue;
                    }
                    if processed + pending.len() >= MAX_PLACEMENTS {
                        placements.incomplete = Some(format!(
                            "more than {MAX_PLACEMENTS} module files are declared"
                        ));
                        return placements;
                    }
                    let opened = version.child(&file, &scope, &name, declaration);
                    if version.limited() {
                        placements.incomplete = Some(format!(
                            "a module file exceeds {} MiB or the module files exceed {} MiB",
                            MAX_WALK_FILE_BYTES / 1024 / 1024,
                            MAX_WALK_BYTES / 1024 / 1024
                        ));
                        return placements;
                    }
                    match opened {
                        Opened::File(child) => {
                            let key = (child_module.len(), child_module, child.path().to_owned());
                            let entry = pending.entry(key).or_insert_with(|| Pending {
                                clauses: Vec::new(),
                                index: Some(child),
                            });
                            entry.clauses.extend(clauses);
                        }
                        Opened::Unparsed(child_path) => {
                            let key = (child_module.len(), child_module, child_path);
                            let entry = pending.entry(key).or_insert_with(|| Pending {
                                clauses: Vec::new(),
                                index: None,
                            });
                            entry.clauses.extend(clauses);
                        }
                        Opened::Unknown => {
                            placements.unknown.entry(child_module).or_default().push(
                                UnknownPlacement {
                                    identity: format!(
                                        "{} {:?} {} {}",
                                        file.path(),
                                        scope,
                                        name,
                                        declaration.resolution_identity()
                                    ),
                                    clauses,
                                },
                            );
                        }
                    }
                }
            }
        }
    }
    placements
}
