//! module の置き場の条件が変わったときに、配下の公開の項目の有る設定を比べ直す (Issue #305)。
//!
//! 項目の有る設定 (#291) は項目と同じファイルの条件だけを見るので、別のファイルの `mod` の宣言の
//! 条件 (`#[cfg(feature = "x")] pub mod api;`) を変えると、差分に無い `api.rs` の公開の項目が一部の
//! 設定から消えても比べない。module の置き場の条件に効く入力 (`mod` の宣言の有る条件・場所の指定・
//! 公開か) が差分で変わった lib crate で、旧新の版の置き場の索引 ([`Placements`]) を作り、置き場と
//! 条件の対応が変わった module の経路ごとに、その配下の公開の項目 (差分に無いファイルも、祖先の
//! ファイルの inline module も) を、crate root からの module の経路・impl の対象・qualname・種類で組に
//! して、「置き場の条件 ∧ 項目の有る条件」の OR を旧新で比べる。
//!
//! - 公開: 経路の `mod` の宣言と囲む inline module が全て `pub` の置き場の宣言を数える。経路は公開でも
//!   宣言を囲む inline module が非公開なら、`pub use` の再公開の判定 (ファイル単位。#291 と同じ) で
//!   決める。経路が非公開の置き場は、同じ module に公開の置き場が無く、規約の場所のファイルのときだけ
//!   ファイル単位の再公開の判定で決め (再公開の経路の条件は #306)、そうでなければ数えない (ファイル
//!   単位の判定では置き場を区別できない)。どれも旧新で同じ規則で決める
//! - 片方の版にしか無い項目は、差分の中のファイルの中身がその項目を消した・足した (ふつうの削除・
//!   追加として出る) なら任せ、置き場が消えた・現れたときだけここで出す。旧新の両方にある項目の、
//!   ファイルごとの削除・追加の候補は除く ([`ModuleAvailability::drops_candidate`])
//! - 置き場の入れ替えで、同じ設定の宣言が、ファイルごとの比較が組にしない別の宣言に切り替わり、契約
//!   (signature と囲む impl・trait の見出し、契約が名指す名前の束縛) が違う・同じと言えないなら、組を
//!   決められない変更 (`rust_declaration_pairing_ambiguous`) にする
//! - どの設定でも着かない置き場 (`cfg(any())` の先) のファイルは、項目を数えず、読めない・マクロでも
//!   探し切れないことにしない
//! - 比べ直した項目は、ファイルごとの有る設定の比較 (#291) から外す
//! - 探し切れない (置き場の場所を決められない、置き場のファイルを読めない・構文エラー、項目の位置の
//!   マクロ、上限、`[lib] path`) ときは、truncation の `rust_module_availability_incomplete` を出す
//! - module の経路が片方の版に無い (`mod` の宣言を消した・足した) ときも、配下の項目の置き場が消えた・
//!   現れたものとして比べる (Issue #314)。その名前を再公開の名前の比較 (#306) が型の名前空間の取り込みの
//!   束縛として比べたなら任せる。マクロや属性マクロがその名前や中身を作り得るなら、探し切れなかったことに
//!   する。差分のファイルのファイルごとの比較 (宣言自身の条件の変化) は、置き場の消えた版を扱わないので
//!   別に出す

use super::rust_availability::AvailabilityChange;
use super::rust_dependencies::placements::{Clause, Placement, Placements};
use super::*;
use crate::models::review::CfgAvailability;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// 比べ直すソースの数・読む量の上限。超えれば探し切れなかったとする。
const MAX_SOURCES: usize = 10_000;
const MAX_SOURCE_BYTES: usize = 64 * 1024 * 1024;

/// 版。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Version {
    Old,
    New,
}

impl Version {
    /// もう一方の版。
    fn other(self) -> Self {
        match self {
            Self::Old => Self::New,
            Self::New => Self::Old,
        }
    }
}

/// 項目の識別: crate root のパス、crate root からの module の経路 (ファイルの module と inline
/// module)、impl の対象のトークン、qualname、種類。
type ItemKey = (String, Vec<String>, Vec<String>, String, String);

/// ファイルの中の項目の識別 (inline module の経路、impl の対象のトークン、qualname、種類)。
type FileItemKey = (String, Vec<String>, String, String);

/// 宣言の並びの有る条件の識別 (宣言ごとの、条件を読めたかと生のトークン。並べ替え済み)。
type AvailabilityKey = Vec<(bool, Vec<String>)>;

/// 宣言の並び (ファイルと、置き場の条件を足した有る条件) の識別。
type Structure = Vec<(String, AvailabilityKey)>;

/// 同じ設定で宣言になる旧新の宣言の組 (添字)。
type Coactive = std::rc::Rc<BTreeSet<(usize, usize)>>;

/// 宣言の並びの有る条件の識別。
fn availability_key(side: &[RustDeclarationContext]) -> AvailabilityKey {
    let mut key: AvailabilityKey = side
        .iter()
        .map(|context| {
            (
                context.availability.is_some(),
                context.availability_raw.clone(),
            )
        })
        .collect();
    key.sort();
    key
}

/// ファイルの公開の項目と、同じ順の宣言の文脈。
struct SourceFacts {
    symbols: Vec<(String, String, String)>,
    declarations: Vec<RustDeclarationContext>,
    /// ファイルの中の項目の識別の集合。
    keys: std::collections::HashSet<FileItemKey>,
}

impl SourceFacts {
    fn new(
        symbols: Vec<(String, String, String)>,
        declarations: Vec<RustDeclarationContext>,
    ) -> Self {
        let keys = symbols
            .iter()
            .zip(&declarations)
            .map(|((name, kind, _), context)| file_item_key(context, name, kind))
            .collect();
        Self {
            symbols,
            declarations,
            keys,
        }
    }
}

/// 宣言のファイルの中の項目の識別。
fn file_item_key(context: &RustDeclarationContext, name: &str, kind: &str) -> FileItemKey {
    (
        context.inline_module.clone(),
        context.owner.clone(),
        name.to_owned(),
        kind.to_owned(),
    )
}

/// inline module の経路 (`a::b`) の名前の並び。
fn inline_segments(context: &RustDeclarationContext) -> Vec<String> {
    context
        .inline_module
        .split("::")
        .filter(|segment| !segment.is_empty())
        .map(str::to_owned)
        .collect()
}

/// 比べ直す項目の宣言 1 つ。
#[derive(Clone)]
struct Entry {
    file: String,
    /// ファイルの索引 (契約が名指す名前の束縛を、その版の crate で解く)。
    index: Option<File>,
    /// (名前, 種類, signature)。
    symbol: (String, String, String),
    context: RustDeclarationContext,
    /// 置き場の条件の節と、その節の置き場でこの宣言が公開か。
    clauses: Vec<(Clause, bool)>,
}

impl Entry {
    /// 公開の節ごとに、置き場の条件を足した宣言の文脈 (有る条件の OR に使う)。
    fn expanded(&self) -> impl Iterator<Item = RustDeclarationContext> + '_ {
        self.clauses
            .iter()
            .filter(|(_, public)| *public)
            .map(|(clause, _)| {
                let mut context = self.context.clone();
                context.availability = clause
                    .attributes
                    .as_ref()
                    .zip(self.context.availability.as_ref())
                    .map(|(chain, own)| chain.iter().chain(own).cloned().collect());
                let mut raw = clause.raw.clone();
                raw.push("|".to_owned());
                raw.extend(self.context.availability_raw.iter().cloned());
                context.availability_raw = raw;
                context
            })
    }

    /// 契約 (signature と囲む impl・trait の見出し)。
    fn contract(&self) -> (&str, &[String]) {
        (&self.symbol.2, &self.context.container_header)
    }

    /// 報告の候補。
    fn candidate(&self) -> ApiSymbolCandidate {
        ApiSymbolCandidate {
            inline_module: inline_module_of(&self.context),
            name: self.symbol.0.clone(),
            kind: self.symbol.1.clone(),
            file: self.file.clone(),
            signature: self.symbol.2.clone(),
        }
    }
}

/// 比べ直した結果の 1 つ。
pub(crate) enum Outcome {
    /// 有る設定の変化 (#291 と同じ区分で振り分ける)。
    Availability {
        change: AvailabilityChange,
        candidate: ApiSymbolCandidate,
        old_signature: String,
        availability: CfgAvailability,
        /// 旧版で公開だった (消える設定を削除と同じ区分にしてよい)。
        removable: bool,
    },
    /// 同じ設定の契約が別のファイルの宣言に切り替わる (組を決められない変更)。
    Ambiguous {
        candidate: ApiSymbolCandidate,
        old_signature: String,
    },
}

/// 比べ直しが扱った項目の様子。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyStatus {
    NotCovered,
    Covered { two_sided: bool },
}

/// (crate root, 置き場の module の経路)。
type PlacedModule = (String, Vec<String>);

/// module の置き場の条件の変化で比べ直した結果と、扱った範囲。
#[derive(Default)]
pub(crate) struct ModuleAvailability {
    outcomes: Vec<Outcome>,
    /// (版, ファイル) → 置き場。比べ直した crate の全ての置き場。
    placements: HashMap<(Version, String), Vec<PlacedModule>>,
    /// crate root → 置き場の条件の対応が変わった module の経路と、探し切ったか。
    changed: HashMap<String, BTreeMap<Vec<String>, bool>>,
    /// 扱った項目と、旧新の両方にあるか。
    keys: HashMap<ItemKey, bool>,
    /// 探し切れなかったことの申告。
    pub(crate) truncations: Vec<crate::models::truncation::TruncationInfo>,
    /// 追加の候補の名前 (差分のほかのファイルで使われているかを索引で引く)。
    names: Vec<String>,
}

impl ModuleAvailability {
    /// 差分の中の package の lib crate のうち、module の置き場の条件に効く入力が変わったものを比べ直す。
    /// `reexports` は再公開した名前の比べ直しの結果 (消した・足した module の名前を任せるかに使う)。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        dir: &str,
        rust_crates: &RustCrates<'_>,
        base_blobs: &crate::commands::git_input::GitBlobBatch,
        strip_internal: &super::ts_strip_internal::StripInternal,
        diff_files: &[crate::models::impact::DiffFile],
        renames: &[crate::models::impact::DiffFile],
        reexports: &super::rust_reexport_names::ReexportNames,
        rust_public: &mut RustPublicApiContext,
    ) -> Self {
        let mut this = Self::default();
        for (root, reason) in rust_crates.module_gaps() {
            this.truncations.push(incomplete(root, reason));
        }
        let pairs: Vec<(String, String)> = diff_files
            .iter()
            .chain(renames)
            .map(|df| (df.old_path.clone(), df.new_path.clone()))
            .collect();
        let diff_paths: std::collections::HashSet<String> = pairs
            .iter()
            .flat_map(|(old_path, new_path)| [old_path.clone(), new_path.clone()])
            .filter(|path| path != "/dev/null")
            .collect();
        for (root, pair) in rust_crates.module_pairs() {
            let mut comparison = CrateComparison {
                dir,
                root: &root,
                pair,
                crates: rust_crates,
                base_blobs,
                strip_internal,
                pairs: &pairs,
                diff_files,
                diff_paths: &diff_paths,
                compared: HashMap::new(),
                coactive: HashMap::new(),
                rust_public,
                reexports,
                facts: HashMap::new(),
                bytes: 0,
            };
            comparison.run(&mut this);
        }
        this
    }

    /// 比べ直した結果。
    pub(crate) fn outcomes(&self) -> &[Outcome] {
        &self.outcomes
    }

    /// 追加の候補の名前 (ApiRefIndex で引く)。
    pub(crate) fn index_names(&self) -> impl Iterator<Item = &String> {
        self.names.iter()
    }

    /// 版 `version` のファイル `path` の宣言 (`context`・名前・種類) を、比べ直しが扱ったか。ファイルの
    /// 全ての置き場で、探し切った module の配下の項目のときだけ扱った。
    fn status(
        &self,
        version: Version,
        path: &str,
        context: &RustDeclarationContext,
        name: &str,
        kind: &str,
    ) -> KeyStatus {
        let Some(placements) = self.placements.get(&(version, path.to_owned())) else {
            return KeyStatus::NotCovered;
        };
        if placements.is_empty() {
            return KeyStatus::NotCovered;
        }
        let mut two_sided = true;
        for (root, module) in placements {
            let mut full = module.clone();
            full.extend(inline_segments(context));
            let complete = self
                .changed
                .get(root)
                .and_then(|changed| deepest_changed(changed, &full));
            if complete != Some(true) {
                return KeyStatus::NotCovered;
            }
            let key = (
                root.clone(),
                full,
                context.owner.clone(),
                name.to_owned(),
                kind.to_owned(),
            );
            match self.keys.get(&key) {
                Some(&both) => two_sided &= both,
                None => return KeyStatus::NotCovered,
            }
        }
        KeyStatus::Covered { two_sided }
    }

    /// 版 `version` のファイル `path` の宣言の、ファイルごとの削除 (旧版) ・追加 (新版) の候補を除くか。
    /// 比べ直しが扱い、旧新の両方にある項目 (置き場の付け替えで別のファイルに残る) なら除く。
    pub(crate) fn drops_candidate(
        &self,
        version: Version,
        path: &str,
        context: &RustDeclarationContext,
        name: &str,
        kind: &str,
    ) -> bool {
        self.status(version, path, context, name, kind) == KeyStatus::Covered { two_sided: true }
    }

    /// 変更ファイル `df` の、同じキーの旧新の宣言 (`old`・`new`、宣言の文脈と名前・種類) の有る設定の
    /// 比較を、比べ直しが扱ったか (ファイルごとの比較を飛ばす)。旧新の全ての置き場で、旧新の両方にある項目と
    /// して比べたときだけ扱ったとする。片方の版にしか無い置き場 (module の宣言を消した・足した、など) が
    /// あれば、その置き場の有無の変化とは別に、ファイルごとの比較 (宣言自身の条件の変化) も出す (Issue #314)。
    pub(crate) fn covers_availability(
        &self,
        df: &crate::models::impact::DiffFile,
        old: &[(&RustDeclarationContext, &str, &str)],
        new: &[(&RustDeclarationContext, &str, &str)],
    ) -> bool {
        let covered =
            |version: Version, path: &str, side: &[(&RustDeclarationContext, &str, &str)]| {
                side.iter().all(|(context, name, kind)| {
                    self.status(version, path, context, name, kind)
                        == KeyStatus::Covered { two_sided: true }
                })
            };
        (!old.is_empty() || !new.is_empty())
            && covered(Version::Old, &df.old_path, old)
            && covered(Version::New, &df.new_path, new)
    }
}

/// 探し切れなかったことの申告。
fn incomplete(root: &str, reason: &str) -> crate::models::truncation::TruncationInfo {
    crate::models::truncation::TruncationInfo::new(
        Some(root.to_owned()),
        crate::models::truncation::TruncationReason::RustModuleAvailabilityIncomplete,
        format!(
            "public items of Rust modules whose declarations changed conditions were not all compared: {reason}"
        ),
    )
}

/// 置き場とその条件の対応 (ファイル → 節の識別の集合)。
fn mapping(placements: Option<&Vec<Placement>>) -> BTreeMap<&str, BTreeSet<(Vec<String>, bool)>> {
    let mut mapping: BTreeMap<&str, BTreeSet<(Vec<String>, bool)>> = BTreeMap::new();
    for placement in placements.into_iter().flatten() {
        mapping
            .entry(placement.file.as_str())
            .or_default()
            .extend(placement.clauses.iter().map(Clause::identity));
    }
    mapping
}

/// 場所を決められない宣言の識別と条件の集合。
fn unknown_mapping(
    placements: &Placements,
    module: &[String],
) -> BTreeSet<(String, Vec<String>, bool)> {
    placements
        .unknown
        .get(module)
        .into_iter()
        .flatten()
        .flat_map(|unknown| {
            unknown.clauses.iter().map(|clause| {
                let (raw, public) = clause.identity();
                (unknown.identity.clone(), raw, public)
            })
        })
        .collect()
}

/// 1 つの crate の比べ直し。
struct CrateComparison<'c, 'a> {
    dir: &'c str,
    root: &'c str,
    pair: &'c super::rust_dependencies::crates::CratePair<'a>,
    /// 差分の中の package の lib crate (移したファイルの行き先を引く。Issue #314)。
    crates: &'c RustCrates<'a>,
    base_blobs: &'c crate::commands::git_input::GitBlobBatch,
    strip_internal: &'c super::ts_strip_internal::StripInternal,
    /// 差分のファイルの (旧いパス, 新しいパス)。
    pairs: &'c [(String, String)],
    /// 差分のファイル (rename だけのものを除く。定義の移動を確かめるのに使う)。
    diff_files: &'c [crate::models::impact::DiffFile],
    /// 差分のファイルのパス (旧新とも)。ほかのパスは旧新で同じ中身。
    diff_paths: &'c std::collections::HashSet<String>,
    /// 有る条件の比較の結果 (旧新の宣言の条件の生のトークンごと)。
    compared: HashMap<(AvailabilityKey, AvailabilityKey), AvailabilityChange>,
    /// 同じ設定で宣言になる旧新の組 (旧新の宣言のファイルと条件の並びごと)。
    coactive: HashMap<(Structure, Structure), Option<Coactive>>,
    rust_public: &'c mut RustPublicApiContext,
    /// 再公開した名前の比べ直しの結果 (消した・足した module の名前を任せるか。Issue #314)。
    reexports: &'c super::rust_reexport_names::ReexportNames,
    /// (版, ファイル) → 公開の項目 (読めない・構文エラーなら None)。
    facts: HashMap<(Version, String), Option<std::rc::Rc<SourceFacts>>>,
    /// 読んだ量。
    bytes: usize,
}

impl CrateComparison<'_, '_> {
    fn placements(&self, version: Version) -> &Placements {
        match version {
            Version::Old => self.pair.old.placements(),
            Version::New => self.pair.new.placements(),
        }
    }

    fn run(&mut self, result: &mut ModuleAvailability) {
        let root = self.root.to_owned();
        for version in [Version::Old, Version::New] {
            if let Some(reason) = &self.placements(version).incomplete {
                result.truncations.push(incomplete(&root, reason));
                return;
            }
        }
        let pair = self.pair;
        let (old, new) = (pair.old.placements(), pair.new.placements());
        // 置き場とその条件の対応が変わった module。片方の版にしか無い module (`mod` の宣言を消した・
        // 足した) も、配下の項目の置き場が消えた・現れたものとして比べる (Issue #314)。
        let modules: BTreeSet<&Vec<String>> = old.files.keys().chain(new.files.keys()).collect();
        let mut changed: BTreeMap<Vec<String>, bool> = BTreeMap::new();
        let mut gaps: Vec<String> = Vec::new();
        for module in modules {
            if mapping(old.files.get(module)) == mapping(new.files.get(module)) {
                continue;
            }
            // 片方の版にしか無い module の名前を、もう一方の版の取り込みが作るなら、その名前は再公開の
            // 名前の比較 (#306) に任せる (`pub use detail as api;` で旧い公開の経路を保つ、など)。
            let missing = if !old.has_module(module) {
                Some(Version::Old)
            } else if !new.has_module(module) {
                Some(Version::New)
            } else {
                None
            };
            // 片方の版にしか無い module の置き場のファイルを、どれも差分が消した (足した) か、別のパスの
            // lib crate の module へ (から) 移したなら、ファイルの比較が中身の項目を削除・追加・移動として
            // 扱う (`mod a;` を `mod b;` にして `a.rs` を `b.rs` へ移す。#314 の前と同じ)。
            if let Some(version) = missing
                && self.left_to_files(version.other(), module)
            {
                continue;
            }
            match missing.map(|version| self.rebinding(version, module)) {
                Some(Rebinding::Imported) => continue,
                Some(Rebinding::Unknown(reason)) => {
                    changed.insert(module.clone(), false);
                    gaps.push(format!("module `{}`: {reason}", module.join("::")));
                }
                Some(Rebinding::Absent) | None => {
                    changed.insert(module.clone(), true);
                    // 消した module の中の属性マクロ・`derive` が作る項目は、見える項目と一緒に消えるが
                    // 数えられない。見える項目の削除は出したうえで、探し切れなかったことを申告する。
                    if missing == Some(Version::New) && self.attributed_under(Version::Old, module)
                    {
                        gaps.push(format!(
                            "module `{}`: items made by attribute macros or derives inside it were not listed",
                            module.join("::")
                        ));
                    }
                }
            }
        }
        // 場所を決められない宣言の条件や場所の指定が変わった。
        let unknown: BTreeSet<&Vec<String>> =
            old.unknown.keys().chain(new.unknown.keys()).collect();
        for module in unknown {
            if unknown_mapping(old, module) != unknown_mapping(new, module) {
                gaps.push(format!(
                    "the file of module `{}` could not be determined",
                    module.join("::")
                ));
            }
        }
        // 条件の変わった module の置き場のファイルを読めない、場所を決められない、項目の位置のマクロ。
        let macros = [macro_scopes(old), macro_scopes(new)];
        for (module, complete) in changed.iter_mut() {
            let reason = [old, new]
                .iter()
                .zip(&macros)
                .find_map(|(placements, macros)| {
                    if placements.unknown.contains_key(module) {
                        return Some("its file could not be determined");
                    }
                    if placements
                        .files
                        .get(module)
                        .into_iter()
                        .flatten()
                        .any(|placement| placement.index.is_none() && !placement.clauses.is_empty())
                    {
                        return Some("its file could not be read or parsed");
                    }
                    invokes_macros_under(macros, module)
                        .then_some("it may define items through macros")
                });
            if let Some(reason) = reason {
                *complete = false;
                gaps.push(format!("module `{}`: {reason}", module.join("::")));
            }
        }
        // 置き場の条件に効く入力が変わったファイルが、場所を決められない宣言の配下かもしれない。
        if (!old.unknown.is_empty() || !new.unknown.is_empty() || self.has_unparsed())
            && self.unplaced_changed_file()
        {
            gaps.push(
                "a changed source may belong to a module whose file could not be determined"
                    .to_owned(),
            );
        }
        // 版ごとの全ての置き場を、扱った範囲の判定のために覚える。
        for (version, placements) in [(Version::Old, old), (Version::New, new)] {
            for (module, files) in &placements.files {
                for placement in files {
                    result
                        .placements
                        .entry((version, placement.file.clone()))
                        .or_default()
                        .push((root.clone(), module.clone()));
                }
            }
        }
        // 条件の変わった module と祖先の置き場のファイルの公開の項目を読む。読めなければその module を
        // 扱わず、読む量の上限を超えればどの module も扱わない。
        let mut over_limit = false;
        let candidates: Vec<Vec<String>> = changed
            .iter()
            .filter(|(_, complete)| **complete)
            .map(|(module, _)| module.clone())
            .collect();
        'modules: for module in candidates {
            for (version, placements) in [(Version::Old, old), (Version::New, new)] {
                let files: Vec<String> = (0..=module.len())
                    .filter_map(|depth| placements.files.get(&module[..depth]))
                    .flatten()
                    .filter(|placement| !placement.clauses.is_empty())
                    .map(|placement| placement.file.clone())
                    .collect();
                for file in files {
                    let read = self.facts(version, &file).is_some();
                    // 片方の版にしか無い項目を、差分のファイルの中身の変化が説明するかを確かめるため、
                    // 差分のファイルはもう一方の版も読む (読めなければ説明しないとする)。
                    if read && let Some((other_version, other)) = self.counterpart(version, &file) {
                        let _ = self.facts(other_version, &other);
                    }
                    if self.facts.len() >= MAX_SOURCES || self.bytes > MAX_SOURCE_BYTES {
                        over_limit = true;
                        break 'modules;
                    }
                    if !read {
                        changed.insert(module.clone(), false);
                        gaps.push(format!("the public items of `{file}` could not be read"));
                    }
                }
            }
        }
        if over_limit {
            gaps.push(format!(
                "more than {MAX_SOURCES} sources or {} MiB would have to be read",
                MAX_SOURCE_BYTES / 1024 / 1024
            ));
            for complete in changed.values_mut() {
                *complete = false;
            }
        }
        if !gaps.is_empty() {
            gaps.sort();
            gaps.dedup();
            result.truncations.push(incomplete(&root, &gaps.join("; ")));
        }
        let entries = self.collect_entries(&changed);
        for (key, (old_entries, new_entries)) in entries {
            let two_sided = !old_entries.is_empty() && !new_entries.is_empty();
            result.keys.insert(key, two_sided);
            if let Some(outcome) = self.compare_key(&old_entries, &new_entries) {
                if let Outcome::Availability {
                    change: AvailabilityChange::Changed { gained: true, .. },
                    candidate,
                    ..
                } = &outcome
                {
                    result.names.push(bare_name(&candidate.name).to_owned());
                }
                result.outcomes.push(outcome);
            }
        }
        result.changed.insert(root, changed);
    }

    /// 版 `version` に無い module `module` の、いちばん外側の無い module の名前を、取り込みが作るか
    /// (Issue #314)。再公開の名前の比較 (#306) がその名前を型の名前空間の取り込みの束縛として比べたなら
    /// 任せる。そうでなければ、名前を宣言したもう一方の版で module か祖先に項目を置き換え得る属性が付いて
    /// いない (配下の項目が展開で変わらない) ことを確かめ、その版の crate の親の module (`#[cfg]` で分けた
    /// 全ての分岐) で名前を解き、名前を作るものを決め切れるかを確かめる ([`classify_rebinding`])。
    fn rebinding(&self, version: Version, module: &[String]) -> Rebinding {
        let placements = self.placements(version);
        let Some(depth) =
            (1..=module.len()).find(|&depth| !placements.has_module(&module[..depth]))
        else {
            return Rebinding::Absent;
        };
        let (parent, name) = (&module[..depth - 1], &module[depth - 1]);
        let decided = self.reexports.rebinds(self.root, parent, name);
        if decided == Some(true) {
            return Rebinding::Imported;
        }
        let (crate_version, other) = match version {
            Version::Old => (&self.pair.old, &self.pair.new),
            Version::New => (&self.pair.new, &self.pair.old),
        };
        let (Opened::File(root), Opened::File(other_root)) = (crate_version.root(), other.root())
        else {
            return Rebinding::Unknown("the crate root could not be read");
        };
        let declaring = Resolver::new(Some(other), other_root, None, None);
        if !declaring.opaque_along(module, None).is_some_and(|found| {
            found
                .iter()
                .all(|conditions| declaring.never_holds(conditions))
        }) {
            return Rebinding::Unknown(
                "an attribute macro on it or an enclosing module may change its items",
            );
        }
        let resolver = Resolver::new(Some(crate_version), root, None, None);
        match resolver.bindings_under(parent, name) {
            Some(candidates) => {
                classify_rebinding(&candidates, name, decided.is_some(), |candidate| {
                    resolver.never_holds(candidate)
                })
            }
            None => Rebinding::Unknown("the module that would hold its name could not be resolved"),
        }
    }

    /// 版 `version` の module `module` の配下 (配下の module のファイルと、祖先のファイルの中の配下の inline
    /// module も) に、どこかの設定で有る (置き場に着く条件・ファイルの先頭の条件・項目自身の条件が両立する)、
    /// 項目を置き換え得る属性 (属性マクロ・`derive` など) の付いた項目があるか (Issue #314)。展開が作る項目は
    /// 数えられない。
    fn attributed_under(&self, version: Version, module: &[String]) -> bool {
        self.placements(version)
            .files
            .iter()
            .filter(|(placed, _)| module.starts_with(placed) || placed.starts_with(module))
            .any(|(placed, files)| {
                files.iter().any(|placement| {
                    let Some(file) = &placement.index else {
                        return false;
                    };
                    file.scope_paths().any(|scope| {
                        let mut full = placed.clone();
                        full.extend(scope.iter().cloned());
                        full.starts_with(module)
                            && !scope_unreachable(file, scope, &placement.clauses)
                            && file
                                .scope_attributed(scope)
                                .iter()
                                .chain(file.scope_body_attributed(scope))
                                .any(|conditions| {
                                    may_hold(&placement.clauses, file.file_conditions(), conditions)
                                })
                    })
                })
            })
    }

    /// 差分に構文エラーで読めない置き場のファイルがあるか (子の module を決められない)。
    fn has_unparsed(&self) -> bool {
        [Version::Old, Version::New].iter().any(|&version| {
            self.placements(version)
                .files
                .values()
                .flatten()
                .any(|placement| placement.index.is_none() && !placement.clauses.is_empty())
        })
    }

    /// 置き場の条件に効く入力が旧新で変わった差分の Rust のファイルのうち、どちらの版の置き場にも
    /// 無いものがあるか。
    fn unplaced_changed_file(&self) -> bool {
        let placed = |version: Version, path: &str| {
            self.placements(version)
                .files
                .values()
                .flatten()
                .any(|placement| placement.file == path)
        };
        self.pairs.iter().any(|(old_path, new_path)| {
            let in_package = |path: &str| {
                super::rust_dependencies::crates::package_of(self.dir, path).as_deref()
                    == Some(self.pair.package.as_str())
            };
            if !in_package(old_path) && !in_package(new_path) {
                return false;
            }
            if placed(Version::Old, old_path) || placed(Version::New, new_path) {
                return false;
            }
            // 無いファイルは `mod` の宣言の無いファイルとして比べる (読めなければ None)。
            let fingerprint = |source: Option<Vec<u8>>, path: &str| match source {
                None => Some(Vec::new()),
                Some(source) => DependencyIndex::new(&source, path)
                    .map(|index| index.module_condition_fingerprint()),
            };
            let old_fingerprint = if old_path == "/dev/null" {
                Some(Vec::new())
            } else {
                fingerprint(self.pair.old.read(old_path), old_path)
            };
            let new_fingerprint = if new_path == "/dev/null" {
                Some(Vec::new())
            } else {
                fingerprint(self.pair.new.read(new_path), new_path)
            };
            old_fingerprint.is_none()
                || new_fingerprint.is_none()
                || old_fingerprint != new_fingerprint
        })
    }

    /// 版 `version` のファイル `path` の公開の項目 (読めない・構文エラー・上限なら None)。
    fn facts(&mut self, version: Version, path: &str) -> Option<std::rc::Rc<SourceFacts>> {
        // 差分に無いファイルは旧新で同じ中身なので、旧版の鍵で共有する。
        let version = if self.diff_paths.contains(path) {
            version
        } else {
            Version::Old
        };
        if let Some(known) = self.facts.get(&(version, path.to_owned())) {
            return known.clone();
        }
        let source = match version {
            Version::Old => self.pair.old.read(path),
            Version::New => self.pair.new.read(path),
        };
        let facts = source.and_then(|source| {
            self.bytes += source.len();
            if self.facts.len() >= MAX_SOURCES || self.bytes > MAX_SOURCE_BYTES {
                return None;
            }
            let (symbols, declarations) =
                extract_deleted_exported_facts(path, &source, self.strip_internal)?;
            let declarations = declarations?;
            (symbols.len() == declarations.len())
                .then(|| std::rc::Rc::new(SourceFacts::new(symbols, declarations)))
        });
        self.facts.insert((version, path.to_owned()), facts.clone());
        facts
    }

    /// 探し切った条件の変わった module の配下の項目の宣言を、旧新ごとに集める (項目の module の経路の、
    /// いちばん深い条件の変わった module が探し切ったもの)。
    #[allow(clippy::type_complexity)]
    fn collect_entries(
        &mut self,
        changed: &BTreeMap<Vec<String>, bool>,
    ) -> BTreeMap<ItemKey, (Vec<Entry>, Vec<Entry>)> {
        let mut entries: BTreeMap<ItemKey, (Vec<Entry>, Vec<Entry>)> = BTreeMap::new();
        // 条件の変わった module の経路と、その祖先の経路 (inline module で配下を持ち得る)。
        let modules: BTreeSet<&[String]> = changed
            .iter()
            .filter(|(_, complete)| **complete)
            .flat_map(|(module, _)| (0..=module.len()).map(move |depth| &module[..depth]))
            .collect();
        if modules.is_empty() {
            return entries;
        }
        // 項目の module の経路の、いちばん深い条件の変わった module が探し切ったものか。
        let owner_complete = |full: &[String]| deepest_changed(changed, full) == Some(true);
        for version in [Version::Old, Version::New] {
            let sources: Vec<(Vec<String>, Placement)> = modules
                .iter()
                .filter_map(|module| {
                    let placements = self.placements(version).files.get(*module)?;
                    Some((module.to_vec(), placements))
                })
                .flat_map(|(module, placements)| {
                    placements
                        .iter()
                        .filter(|placement| !placement.clauses.is_empty())
                        .map(move |placement| (module.clone(), placement.clone()))
                        .collect::<Vec<_>>()
                })
                .collect();
            for (module, placement) in sources {
                let Some(facts) = self.facts(version, &placement.file) else {
                    continue;
                };
                // 規約の場所で、同じ module に公開の置き場が無いときだけ、ファイル単位の再公開の判定を使える。
                let conventional = super::source_module_path(&placement.file)
                    .is_some_and(|(_, conventional)| conventional == module);
                let mixed = self
                    .placements(version)
                    .files
                    .get(&module)
                    .into_iter()
                    .flatten()
                    .any(|other| other.clauses.iter().any(|clause| clause.public));
                // 同じファイルの公開の `use` が持ち込む名前 (どの名前も持ち込み得るなら None)。
                let mentions = placement.index.as_ref().map(|index| {
                    index.public_mentions(|scope| {
                        !scope_unreachable(index, scope, &placement.clauses)
                    })
                });
                for ((name, kind, signature), context) in
                    facts.symbols.iter().zip(&facts.declarations)
                {
                    let mut full = module.clone();
                    full.extend(inline_segments(context));
                    if !owner_complete(&full) {
                        continue;
                    }
                    let mut clauses = Vec::new();
                    let mut reexported: Option<bool> = None;
                    for clause in &placement.clauses {
                        let public = if clause.public && context.inline_public {
                            true
                        } else if clause.public {
                            // 経路は公開で、宣言を囲む inline module が非公開: 同じファイルの公開の
                            // `use` が、項目か、項目を囲む inline module を持ち込み得るときだけ数える
                            // (公開の glob・展開できない `use`・マクロがあれば数える)。
                            match &mentions {
                                Some(Some(names)) => {
                                    names.contains(super::rust_reexport_item_name(name))
                                        || inline_segments(context)
                                            .iter()
                                            .any(|segment| names.contains(segment))
                                }
                                Some(None) => true,
                                None => false,
                            }
                        } else if mixed || !conventional {
                            false
                        } else {
                            *reexported.get_or_insert_with(|| {
                                self.reexported(version, &placement.file, name)
                            })
                        };
                        clauses.push((clause.clone(), public));
                    }
                    let key = (
                        self.root.to_owned(),
                        full,
                        context.owner.clone(),
                        name.clone(),
                        kind.clone(),
                    );
                    let entry = Entry {
                        file: placement.file.clone(),
                        index: placement.index.clone(),
                        symbol: (name.clone(), kind.clone(), signature.clone()),
                        context: context.clone(),
                        clauses,
                    };
                    let slot = entries.entry(key).or_default();
                    match version {
                        Version::Old => slot.0.push(entry),
                        Version::New => slot.1.push(entry),
                    }
                }
            }
        }
        entries
    }

    /// 非公開の module の経路の置き場の項目 `name` (ファイル `path`) が、`pub use` の再公開で公開か
    /// (ファイル単位の判定。決められなければ公開)。
    fn reexported(&mut self, version: Version, path: &str, name: &str) -> bool {
        match version {
            Version::Old => !is_rust_old_symbol_outside_public_api_surface(
                self.dir,
                self.base_blobs,
                path,
                name,
                self.rust_public,
            ),
            Version::New => !is_rust_new_symbol_outside_public_api_surface(
                self.dir,
                path,
                name,
                self.rust_public,
            ),
        }
    }

    /// 旧版の宣言 `before` と新版の宣言 `after` を、ファイルごとの比較が組にして比べるか (同じファイル
    /// (同じパスか、差分の rename) の、同じ inline module・impl の対象・名前・種類の宣言)。
    fn compared_by_file(&self, before: &Entry, after: &Entry) -> bool {
        let same_file = before.file == after.file
            || self
                .pairs
                .iter()
                .any(|(old, new)| *old == before.file && *new == after.file);
        same_file
            && file_item_key(&before.context, &before.symbol.0, &before.symbol.1)
                == file_item_key(&after.context, &after.symbol.0, &after.symbol.1)
    }

    /// 版 `version` の差分のファイル `path` の、もう一方の版とそのパス (削除・追加なら None)。差分に
    /// 無いファイルは None。
    fn counterpart(&self, version: Version, path: &str) -> Option<(Version, String)> {
        self.pairs.iter().find_map(|(old_path, new_path)| {
            let (other_version, other) = match version {
                Version::Old if old_path == path => (Version::New, new_path),
                Version::New if new_path == path => (Version::Old, old_path),
                _ => return None,
            };
            (other != "/dev/null").then(|| (other_version, other.clone()))
        })
    }

    /// 版 `version` の差分のファイル `path` の、もう一方の版のパス (削除・追加なら `/dev/null`)。差分に
    /// 無いファイルは None。
    fn diff_counterpart(&self, version: Version, path: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find_map(|(old_path, new_path)| match version {
                Version::Old if old_path == path => Some(new_path.as_str()),
                Version::New if new_path == path => Some(old_path.as_str()),
                _ => None,
            })
    }

    /// 版 `version` にだけある module `module` の置き場のファイル (どこかの設定で着くもの) が、どれも
    /// ファイルの比較に任せられるか ([`Self::carried_by_file`])。置き場が無ければ任せない (Issue #314)。
    fn left_to_files(&mut self, version: Version, module: &[String]) -> bool {
        let files: Vec<String> = self
            .placements(version)
            .files
            .get(module)
            .into_iter()
            .flatten()
            .filter(|placement| !placement.clauses.is_empty())
            .map(|placement| placement.file.clone())
            .collect();
        !files.is_empty() && files.iter().all(|file| self.carried_by_file(version, file))
    }

    /// 版 `version` のファイル `path` を、差分が消した (新しい版なら足した) か、もう一方の版でどこかの
    /// lib crate の module のファイルになる別のパスへ (から) 移したか。ファイルの比較が中身の項目を削除・
    /// 追加・移動として扱い、移したファイルの項目は移した先の module に残るものとする (#314 の前と同じ。
    /// 移した先の module の条件や公開範囲の違いは比べない)。消した (足した) ファイルは、ファイルごとの比較が
    /// 公開の項目をどれも出すときだけ任せる ([`Self::file_items_reported`])。
    fn carried_by_file(&mut self, version: Version, path: &str) -> bool {
        match self.diff_counterpart(version, path).map(str::to_owned) {
            Some(other) if other == "/dev/null" => self.file_items_reported(version, path),
            Some(other) => other != path && self.placed_in_library(version.other(), &other),
            None => false,
        }
    }

    /// 版 `version` のファイル `path` の公開の項目を、ファイルごとの比較がどれも公開 API 面の中として出すか
    /// ([`Self::reexported`]。bin だけの package のファイルや、別の lib の非公開の module のファイルの項目は
    /// 出さない)。読めなければ出さないとする (Issue #314)。
    fn file_items_reported(&mut self, version: Version, path: &str) -> bool {
        let Some(facts) = self.facts(version, path) else {
            return false;
        };
        let names: Vec<String> = facts
            .symbols
            .iter()
            .map(|(name, _, _)| name.clone())
            .collect();
        names
            .iter()
            .all(|name| self.reexported(version, path, name))
    }

    /// ファイル `path` が、版 `version` で、差分の中の package のどれかの lib crate の module のファイルとして、
    /// どこかの設定で着くか。ファイルの置かれたディレクトリの package に限らず、`#[path]` で別の場所のファイルを
    /// 読む crate も見る (置き場を決められない crate では着かないとする)。
    fn placed_in_library(&self, version: Version, path: &str) -> bool {
        self.crates.library_pairs().any(|pair| {
            let krate = match version {
                Version::Old => &pair.old,
                Version::New => &pair.new,
            };
            krate
                .placements()
                .files
                .values()
                .flatten()
                .any(|placement| placement.file == path && !placement.clauses.is_empty())
        })
    }

    /// 片方の版にしか無い項目の宣言 `entry` (版 `version`) を、差分の中のファイルの中身の変化
    /// (ふつうの削除・追加) が説明し、ファイルごとの比較がそれを出すか。
    fn explained_by_file(&mut self, version: Version, entry: &Entry) -> bool {
        let Some(other) = self
            .diff_counterpart(version, &entry.file)
            .map(str::to_owned)
        else {
            return false;
        };
        let key = file_item_key(&entry.context, &entry.symbol.0, &entry.symbol.1);
        // もう一方の版のファイルに無ければ、ファイルの比較がふつうの削除・追加として出す。読めなければ
        // ファイルの比較も出せないので、説明しないとする。
        let explained = other == "/dev/null"
            || self
                .facts(version.other(), &other)
                .is_some_and(|facts| !facts.keys.contains(&key));
        // ファイルごとの比較が公開 API 面の外として出さない削除・追加 (bin だけの package のファイルなど) は、
        // ここで比べる (Issue #314)。
        explained && self.reexported(version, &entry.file, &entry.symbol.0)
    }

    /// 片方の版にしか無い項目の宣言 `entries` (版 `version`) のうち、差分の中のファイルの中身の変化が説明
    /// しないもの ([`Self::explained_by_file`])。説明する宣言はファイルごとの比較が削除・追加として出すが、
    /// ほかの宣言の置き場が消えた・現れたことは出さないので、残りは比べる (Issue #314)。
    fn unexplained_by_file(&mut self, version: Version, entries: &[Entry]) -> Vec<Entry> {
        let mut unexplained = Vec::new();
        for entry in entries {
            if !self.explained_by_file(version, entry) {
                unexplained.push(entry.clone());
            }
        }
        unexplained
    }

    /// 項目の旧新の宣言を比べる。
    fn compare_key(&mut self, old: &[Entry], new: &[Entry]) -> Option<Outcome> {
        let public = |entries: &[Entry]| {
            entries
                .iter()
                .any(|entry| entry.expanded().next().is_some())
        };
        if !public(old) && !public(new) {
            return None;
        }
        let unexplained: Vec<Entry>;
        let (old, new) = match (old.is_empty(), new.is_empty()) {
            (false, true) => {
                unexplained = self.unexplained_by_file(Version::Old, old);
                (unexplained.as_slice(), new)
            }
            (true, false) => {
                unexplained = self.unexplained_by_file(Version::New, new);
                (old, unexplained.as_slice())
            }
            _ => (old, new),
        };
        if !old.is_empty() && !new.is_empty() && self.contract_switches(old, new) {
            let entry = &new[0];
            return Some(Outcome::Ambiguous {
                candidate: entry.candidate(),
                old_signature: old[0].symbol.2.clone(),
            });
        }
        let old_expanded: Vec<RustDeclarationContext> =
            old.iter().flat_map(Entry::expanded).collect();
        let new_expanded: Vec<RustDeclarationContext> =
            new.iter().flat_map(Entry::expanded).collect();
        if old_expanded.is_empty() && new_expanded.is_empty() {
            return None;
        }
        let old_refs: Vec<&RustDeclarationContext> = old_expanded.iter().collect();
        let new_refs: Vec<&RustDeclarationContext> = new_expanded.iter().collect();
        let change = *self
            .compared
            .entry((
                availability_key(&old_expanded),
                availability_key(&new_expanded),
            ))
            .or_insert_with(|| super::rust_availability::compare(&old_refs, &new_refs));
        if change == AvailabilityChange::Same {
            return None;
        }
        let representative = new.first().or_else(|| old.first())?;
        Some(Outcome::Availability {
            change,
            candidate: representative.candidate(),
            old_signature: old.first().map_or_else(
                || representative.symbol.2.clone(),
                |entry| entry.symbol.2.clone(),
            ),
            availability: super::rust_availability::describe(&old_refs, &new_refs),
            removable: !old_expanded.is_empty(),
        })
    }

    /// 同じ設定で、旧版の宣言と新版の宣言が、ファイルごとの比較が組にしない別の宣言に切り替わり、契約
    /// (signature と囲む impl・trait の見出し、契約が名指す名前の束縛) が違うか、同じと言えないか。両方の
    /// 版でその設定の宣言が 1 つに決まる設定だけを見る (2 つ並ぶ設定はビルドできない)。
    fn contract_switches(&mut self, old: &[Entry], new: &[Entry]) -> bool {
        let candidates: Vec<(usize, usize)> = (0..old.len())
            .flat_map(|before| (0..new.len()).map(move |after| (before, after)))
            .filter(|&(before, after)| !self.compared_by_file(&old[before], &new[after]))
            .collect();
        if candidates.is_empty() {
            return false;
        }
        // 決められなければ、全ての組を切り替わり得るものとして調べる。
        let coactive = self.coactive(old, new);
        for (before, after) in candidates {
            if coactive
                .as_ref()
                .is_some_and(|pairs| !pairs.contains(&(before, after)))
            {
                continue;
            }
            let (before, after) = (&old[before], &new[after]);
            if before.contract() != after.contract()
                || !same_declaration(before, after)
                || !self.same_bindings(before, after)
            {
                return true;
            }
        }
        false
    }

    /// 旧新の宣言の組のうち、ある設定で両方の版の宣言がそれぞれ 1 つに決まり、その宣言どうしになる組。
    /// 読めない条件か原子が多すぎれば None。宣言のファイルと条件の並びが同じなら結果を使い回す。
    fn coactive(&mut self, old: &[Entry], new: &[Entry]) -> Option<Coactive> {
        let structure = |entries: &[Entry]| -> Structure {
            entries
                .iter()
                .map(|entry| {
                    let expanded: Vec<RustDeclarationContext> = entry.expanded().collect();
                    (entry.file.clone(), availability_key(&expanded))
                })
                .collect()
        };
        let key = (structure(old), structure(new));
        if let Some(known) = self.coactive.get(&key) {
            return known.clone();
        }
        let computed = coactive_pairs(old, new).map(std::rc::Rc::new);
        self.coactive.insert(key, computed.clone());
        computed
    }

    /// 旧版の宣言 `before` と新版の宣言 `after` の契約が名指す名前の束縛が同じか (それぞれの版の crate で
    /// 解く。#283・#300)。解けなければ同じと言えない。
    fn same_bindings(&self, before: &Entry, after: &Entry) -> bool {
        let (Some(old_file), Some(new_file)) = (&before.index, &after.index) else {
            return false;
        };
        let pair = self.pair;
        let old = Resolver::new(
            Some(&pair.old as &dyn CrateFiles),
            old_file.clone(),
            None,
            None,
        );
        // 定義の移動は、ファイルごとの比較と同じく、差分の中の移動の記録と差分のファイルで確かめる。
        let new = Resolver::new(
            Some(&pair.new as &dyn CrateFiles),
            new_file.clone(),
            Some(&pair.moves),
            None,
        );
        let files = DiffModules::new(
            &after.file,
            self.diff_files,
            Box::new(|path: &str| pair.old.read(path)),
            Box::new(|path: &str| pair.new.read(path)),
        );
        compare_dependencies(
            &old,
            std::slice::from_ref(&before.context),
            &new,
            std::slice::from_ref(&after.context),
            &files,
        ) == DependencyComparison::Same
    }
}

/// 旧版の宣言 `before` と新版の宣言 `after` の、関数の本体を除く宣言全体のトークンが同じか (signature
/// が先頭の行だけの型別名の右辺や、構造体のフィールドも比べる)。片方しか引けなければ同じと言えない。
fn same_declaration(before: &Entry, after: &Entry) -> bool {
    let contract = |entry: &Entry| {
        entry
            .index
            .as_ref()
            .and_then(|index| index.declaration_contract(entry.context.start))
    };
    match (contract(before), contract(after)) {
        (Some(before), Some(after)) => before == after,
        (None, None) => true,
        _ => false,
    }
}

/// 旧新の宣言 (`old`・`new`) の組のうち、ある設定で両方の版の宣言がそれぞれ 1 つに決まり、その宣言
/// どうしになる組 (宣言の添字)。読めない条件か原子が多すぎれば None。
fn coactive_pairs(old: &[Entry], new: &[Entry]) -> Option<BTreeSet<(usize, usize)>> {
    use super::rust_cfg_predicate::{Atoms, Predicate};
    let mut atoms = Atoms::default();
    let mut branches = |entries: &[Entry]| -> Option<Vec<(usize, Predicate)>> {
        let mut branches = Vec::new();
        for (index, entry) in entries.iter().enumerate() {
            for context in entry.expanded() {
                let attributes = context.availability.as_ref()?;
                branches.push((
                    index,
                    Predicate::All(
                        attributes
                            .iter()
                            .map(|attribute| atoms.presence(&attribute.key))
                            .collect(),
                    ),
                ));
            }
        }
        Some(branches)
    };
    let old_branches = branches(old)?;
    let new_branches = branches(new)?;
    let configs = atoms.configs()?;
    let active = |branches: &[(usize, Predicate)], config: u64| -> Option<usize> {
        let mut found: Option<usize> = None;
        for (index, predicate) in branches {
            if predicate.holds(config) {
                if found.is_some_and(|known| known != *index) {
                    return None;
                }
                found = Some(*index);
            }
        }
        found
    };
    let mut pairs = BTreeSet::new();
    for config in configs {
        if let (Some(before), Some(after)) =
            (active(&old_branches, config), active(&new_branches, config))
        {
            pairs.insert((before, after));
        }
    }
    Some(pairs)
}

/// 版の置き場 `placements` で、項目の位置でマクロを呼び出すスコープの、crate root からの module の経路
/// (どの設定でも無いスコープは除く)。置き場の module の経路ごと。
fn macro_scopes(placements: &Placements) -> BTreeMap<Vec<String>, Vec<Vec<String>>> {
    let mut found: BTreeMap<Vec<String>, Vec<Vec<String>>> = BTreeMap::new();
    for (placed, files) in &placements.files {
        for placement in files {
            let Some(file) = &placement.index else {
                continue;
            };
            if placement.clauses.is_empty() {
                continue;
            }
            for scope in file.scope_paths() {
                let holds = |conditions: &Vec<String>| {
                    may_hold(&placement.clauses, file.file_conditions(), conditions)
                };
                let mut calls = file
                    .scope_macro_conditions(scope)
                    .iter()
                    .chain(file.scope_body_macro_conditions(scope))
                    .peekable();
                // 直下と `impl`・trait・`extern` ブロックの中の呼び出しごとに、置き場に着く条件・ファイルの
                // 先頭の条件と両立するものだけを数える。属性マクロは中の `#[cfg]` を解かずに受け取るので、
                // 適用され得る属性マクロに囲まれた呼び出しは、呼び出しと間の inline module の条件で除かない
                // (Issue #314)。
                let wrapped = calls.peek().is_some() && file.scope_opaque(scope).iter().any(holds);
                if wrapped
                    || (calls.any(holds) && !scope_unreachable(file, scope, &placement.clauses))
                {
                    let mut full = placed.clone();
                    full.extend(scope.iter().cloned());
                    found.entry(placed.clone()).or_default().push(full);
                }
            }
        }
    }
    found
}

/// 置き場に着く条件の節 `clauses` のどれかと、ファイルの先頭の条件 `file`、項目自身の条件 `own` (どれも条件の
/// 属性のトークン) が、どこかの設定で同時に成り立ち得るか (成り立たないと確かめられなければ真。節が無ければ
/// 偽。Issue #314)。
fn may_hold(clauses: &[Clause], file: &[String], own: &[String]) -> bool {
    clauses.iter().any(|clause| {
        let mut all: Vec<String> = clause.raw.clone();
        all.extend(file.iter().cloned());
        all.extend(own.iter().cloned());
        !super::rust_dependencies::conditions_never_hold(&all)
    })
}

/// module `module` の置き場のファイルか、その祖先の置き場のファイルの `module` の配下のスコープが、
/// 項目の位置でマクロを呼び出すか (`scopes` は [`macro_scopes`] の結果)。
fn invokes_macros_under(
    scopes: &BTreeMap<Vec<String>, Vec<Vec<String>>>,
    module: &[String],
) -> bool {
    (0..=module.len()).any(|depth| {
        scopes
            .get(&module[..depth])
            .is_some_and(|found| found.iter().any(|full| full.starts_with(module)))
    })
}

/// 片方の版にしか無い module の名前を、その版の取り込みが作るか。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rebinding {
    /// 作らない (配下の項目の置き場が消えた・現れたものとして比べる)。
    Absent,
    /// 再公開の名前の比較 (#306) が、型の名前空間の取り込みの束縛として比べた (公開範囲と条件を問わない)。
    Imported,
    /// 決められない (理由)。
    Unknown(&'static str),
}

/// 名前解決の候補 `candidates` (名前 `name` を解いたもの) から、名前を作るものを決め切れるかを確かめる。
/// prelude の候補と、どの設定でも成り立たない条件の候補 (`never`) は数えない。glob の先・マクロ・属性
/// マクロ・読めないファイル・解けない経路と、項目を置き換え得る属性の付いた同じ名前の定義は決め切れない。
/// 再公開の名前の比較 (#306) が名前を比べ切った (`decided`) なら、ほかの束縛 (宣言、#306 が型の名前空間の
/// 取り込みの束縛にならないとした取り込み) は module の名前を作らない。比べ切っていなければ、取り込み
/// (親のスコープの定義でない候補) があれば決め切れない。
fn classify_rebinding(
    candidates: &[super::rust_dependencies::Candidate],
    name: &str,
    decided: bool,
    never: impl Fn(&[String]) -> bool,
) -> Rebinding {
    use super::rust_dependencies::Target;
    const UNCERTAIN: [(&str, &str); 8] = [
        (
            "glob:",
            "a glob whose module could not be determined may bring its name",
        ),
        (
            "macros:",
            "a macro call at item level in its parent may make its name",
        ),
        (
            "attributes:",
            "an item with an attribute macro or derive in its parent may make its name",
        ),
        (
            "opaque:",
            "an attribute macro on its parent, an enclosing module, or a `mod` on the way may make its name",
        ),
        (
            "unparsed:",
            "a file that could not be parsed may make its name",
        ),
        (
            "mod-file:",
            "a path that could not be resolved may make its name",
        ),
        (
            "outside:",
            "a path that could not be resolved may make its name",
        ),
        (
            "crate:",
            "a path that could not be resolved may make its name",
        ),
    ];
    let prelude = format!("prelude:{name}");
    for candidate in candidates {
        if matches!(&candidate.target, Target::Identity(identity) if *identity == prelude)
            || never(&candidate.conditions)
        {
            continue;
        }
        if let Target::Identity(identity) = &candidate.target
            && let Some((_, reason)) = UNCERTAIN
                .iter()
                .find(|(prefix, _)| identity.starts_with(prefix))
        {
            return Rebinding::Unknown(reason);
        }
        // 置き換え得る属性は、定義の条件と、属性が適用される条件 (`cfg_attr` の述語) がどこかの設定で両立する
        // ものだけを数える。
        if candidate.replaceable.iter().any(|applied| {
            let mut all = candidate.conditions.clone();
            all.extend(applied.iter().cloned());
            !never(&all)
        }) {
            return Rebinding::Unknown(
                "a definition of the same name has an attribute that may replace it",
            );
        }
        if !decided && !candidate.direct {
            return Rebinding::Unknown(
                "an import whose comparison of re-exported names did not finish may make its name",
            );
        }
    }
    Rebinding::Absent
}

/// 置き場の条件 `clauses` で着くファイル `file` のスコープ `scope` (inline module の経路) が、どの設定
/// でも無いと確かめられるか: どの節でも、ファイル自身の先頭の `#![cfg]` か、経路のどれかの段の inline
/// module の宣言の全て (囲む module・ファイルの先頭・中身の先頭の内側の属性を含む) と成り立たない。
fn scope_unreachable(file: &DependencyIndex, scope: &[String], clauses: &[Clause]) -> bool {
    let levels: Vec<Vec<&ModuleDeclaration>> = (1..=scope.len())
        .map(|depth| {
            file.module_declarations(&scope[..depth - 1], &scope[depth - 1])
                .into_iter()
                .filter(|declaration| declaration.inline())
                .collect()
        })
        .collect();
    let never = |chain: &[AvailabilityAttr], own: &[AvailabilityAttr]| {
        let combined: Vec<AvailabilityAttr> = chain.iter().chain(own).cloned().collect();
        super::rust_availability::unsatisfiable(&combined)
    };
    !clauses.is_empty()
        && clauses.iter().all(|clause| {
            let Some(chain) = &clause.attributes else {
                return false;
            };
            file.file_availability()
                .is_some_and(|own| never(chain, own))
                || levels.iter().any(|declarations| {
                    !declarations.is_empty()
                        && declarations.iter().all(|declaration| {
                            declaration
                                .availability()
                                .is_some_and(|own| never(chain, own))
                        })
                })
        })
}

/// 項目の module の経路 `full` の、いちばん深い条件の変わった module の、探し切ったか (無ければ None)。
fn deepest_changed(changed: &BTreeMap<Vec<String>, bool>, full: &[String]) -> Option<bool> {
    (0..=full.len())
        .rev()
        .find_map(|depth| changed.get(&full[..depth]).copied())
}
