//! Rust の公開の項目の有る設定の比較 (#291。別のファイルへの移動・rename も) に、ファイルを置く module の
//! 置き場の条件を足す (Issue #316)。
//!
//! 項目の有る設定は、項目自身と同じファイルの囲む項目・ファイルの先頭の `#![cfg]` の条件 ([`RustDeclarationContext::availability`])
//! に、そのファイルを置く module の経路の条件 (crate root からその module の宣言までの `#[cfg]`・`cfg` に絞った
//! `#[cfg_attr]`・途中のファイルの先頭の `#![cfg]`。[`Clause`]) を足したもの。移動元の旧い置き場と移動先の新しい
//! 置き場の公開の経路の節で、宣言の条件の前に節の属性を足して比べる (#305 の比べ直しと同じ)。同じファイルの中の
//! 変化は、旧新とも同じ唯一の lib crate の同じ唯一の公開 module に置かれ、展開・読取失敗による不明な置き場がない
//! と確かめられるときだけ、その実効条件で比べる。module の条件に既に含まれる冗長な項目の条件は変化にしない。
//!
//! 移動元と移動先を同じ項目として比べるのは、同じ lib crate の中で、旧新に公開の経路の module が 1 つずつあり、
//! 移動元の経路を移動元のファイルだけが、移動先の経路を移動先のファイルだけが (旧新とも) 置くときだけ (同じファイルを
//! 複数の module に置く、cfg で同じ経路に別のファイルを置くなら、経路ごとに別の項目になり得るので比べない)。
//! inherent impl・trait の項目は、置いた module ではなく対象の型・trait で外から使えるかが決まり、型が外から使える
//! 条件は設定と置き場で違い得るので、置き場の条件では比べない。
//!
//! 別のファイルへの移動では、置き場の条件を足した比較は、今までの比較 (宣言自身の条件) の報告に、見つかった変化を
//! 足すことにだけ使う。同じファイルでも置き場の一意性・完全性を証明できなければ、今までの報告を減らさない。
//! 非公開の経路の置き場は足さない (外から使えるのは `pub use` の再公開を
//! 通したときで、その条件は再公開の名前の比較 (#306) が比べる)。置き場を決められず、移動元と移動先で置き場の条件が
//! 違い得るなら、今までの比較が変化を見つけていなければ、比べられなかったもの (blocking) にする。

use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::RustDeclarationContext;
use super::rust_availability::{AvailabilityChange, compare, describe};
use super::rust_dependencies::RustCrates;
use super::rust_dependencies::placements::Clause;
use crate::models::review::CfgAvailability;

/// lib crate の中のファイルの置き場 (module の経路 → その経路に着く節)。
type Modules = BTreeMap<Vec<String>, Vec<Clause>>;

/// 版の lib crate の置き場の逆引き。
#[derive(Default)]
struct PlacementIndex {
    /// ファイル → lib crate の root → module の経路 → 置き場の節。
    files: HashMap<String, BTreeMap<String, Modules>>,
    /// lib crate の root と module の経路 → その経路に置くファイル。
    modules: HashMap<(String, Vec<String>), BTreeSet<String>>,
    /// 置き場を決められない lib crate がある (lib の root が旧新で違う package は数えない)。
    unknown: bool,
    /// マクロ・属性マクロ・読めないファイルがあり、列挙した置き場だけとは証明できない。
    uncertain: bool,
}

/// 片側のファイルの置き場。
enum Side<'i> {
    /// lib crate の module のファイルとして置かれる (lib crate の root → module の経路 → 置き場の節)。
    Placed(&'i BTreeMap<String, Modules>),
    /// 調べたどの lib crate の module のファイルでもない (bin の module など)。
    Outside,
    /// どこにも置かれていないが、置き場を決められない lib crate がある。
    Unknown,
}

/// 置き場の条件を足した有る設定の比較の結果。
pub(crate) struct Decision {
    /// 今までの比較の区分に、置き場を足して見つかった変化を足した区分。
    pub(crate) change: AvailabilityChange,
    /// 置き場を足して初めて見つかった、消える設定の表示 (今までの比較が消える設定を見つけていれば None)。
    lost: Option<CfgAvailability>,
    /// 置き場を足して初めて見つかった、増える設定の表示 (今までの比較が増える設定を見つけていれば None)。
    gained: Option<CfgAvailability>,
}

impl Decision {
    /// 置き場を足さない区分 `change` (今までの表示のまま)。
    pub(crate) fn plain(change: AvailabilityChange) -> Self {
        Self {
            change,
            lost: None,
            gained: None,
        }
    }

    /// 消える設定の表示と、増える設定の表示 (消える設定と同じなら None)。置き場を足して初めて見つかった向きは
    /// 置き場の条件を含め、ほかの向きは今までの表示 `plain` にする (lib crate ごとに見つけた向きの表示を混ぜない)。
    pub(crate) fn displays(
        self,
        plain: impl FnOnce() -> CfgAvailability,
    ) -> (CfgAvailability, Option<CfgAvailability>) {
        match (self.lost, self.gained) {
            (None, None) => (plain(), None),
            (Some(lost), Some(gained)) => (lost, Some(gained)),
            (Some(lost), None) => (lost, Some(plain())),
            (None, Some(gained)) => (plain(), Some(gained)),
        }
    }
}

/// 置き場の条件を足した有る設定の比較 (Issue #316)。
pub(crate) struct PlacementAvailability<'c, 'a> {
    crates: &'c RustCrates<'a>,
    /// 旧い版・新しい版の置き場の逆引き。
    old: OnceCell<PlacementIndex>,
    new: OnceCell<PlacementIndex>,
}

impl<'c, 'a> PlacementAvailability<'c, 'a> {
    pub(crate) fn new(crates: &'c RustCrates<'a>) -> Self {
        Self {
            crates,
            old: OnceCell::new(),
            new: OnceCell::new(),
        }
    }

    /// 旧い版 (`old`) か新しい版の、差分の中の package の lib crate の置き場の逆引き。
    fn index(&self, old: bool) -> &PlacementIndex {
        let cell = if old { &self.old } else { &self.new };
        cell.get_or_init(|| {
            let mut index = PlacementIndex::default();
            for krate in self.crates.placement_versions(old) {
                let placements = krate.placements();
                if placements.incomplete.is_some() || !placements.unknown.is_empty() {
                    index.unknown = true;
                    continue;
                }
                let root = krate.root_path();
                for (module, placed) in &placements.files {
                    for placement in placed {
                        index.uncertain |= placement.index.as_ref().is_none_or(|file| {
                            file.scope_paths().any(|scope| {
                                !file.scope_macro_conditions(scope).is_empty()
                                    || !file.scope_body_macro_conditions(scope).is_empty()
                                    || !file.scope_opaque(scope).is_empty()
                                    || !file.scope_attributed(scope).is_empty()
                                    || !file.scope_body_attributed(scope).is_empty()
                            })
                        });
                        index
                            .files
                            .entry(placement.file.clone())
                            .or_default()
                            .entry(root.to_owned())
                            .or_default()
                            .entry(module.clone())
                            .or_default()
                            .extend(placement.clauses.iter().cloned());
                        index
                            .modules
                            .entry((root.to_owned(), module.clone()))
                            .or_default()
                            .insert(placement.file.clone());
                    }
                }
            }
            index
        })
    }

    /// 旧い版 (`old`) か新しい版のファイル `path` の置き場。
    fn side(&self, old: bool, path: &str) -> Side<'_> {
        let index = self.index(old);
        match index.files.get(path) {
            Some(crates) => Side::Placed(crates),
            None if index.unknown => Side::Unknown,
            None => Side::Outside,
        }
    }

    /// 旧い版 (`old`) か新しい版で、lib crate `root` の module の経路 `module` を置くファイルが、`file` だけか
    /// (経路が無ければ真)。
    fn only(&self, old: bool, root: &str, module: &[String], file: &str) -> bool {
        self.index(old)
            .modules
            .get(&(root.to_owned(), module.to_vec()))
            .is_none_or(|files| files.iter().all(|placed| placed == file))
    }

    /// 旧い宣言 `old` (旧い版のファイル `old_path`) を新しい宣言 `new` (新しい版のファイル `new_path`) へ移した
    /// ときの有る設定を、ファイルを置く module の置き場の条件も足して比べる。同じファイルの中の変化は、一意で
    /// 確定した公開の置き場なら実効条件で置き換え、それ以外は今までの比較 `plain` のまま。
    pub(crate) fn decide(
        &self,
        old_path: &str,
        old: &[&RustDeclarationContext],
        new_path: &str,
        new: &[&RustDeclarationContext],
        plain: AvailabilityChange,
    ) -> Decision {
        if old_path == new_path {
            return self
                .same_placement(old_path, old, new)
                .unwrap_or_else(|| Decision::plain(plain));
        }
        match (self.side(true, old_path), self.side(false, new_path)) {
            (Side::Placed(old_crates), Side::Placed(new_crates)) => {
                // 旧新を同じ lib crate どうしで比べる (別の crate の条件を混ぜない)。旧新の片方の版にしか置かない
                // crate では、その crate の公開の項目から出入りするので、置き場の条件では比べない。
                let mut placed = AvailabilityChange::Same;
                let mut lost = None;
                let mut gained = None;
                for (root, old_modules) in old_crates {
                    let Some(new_modules) = new_crates.get(root) else {
                        continue;
                    };
                    let (Some((old_module, old_clauses)), Some((new_module, new_clauses))) =
                        (single(old_modules), single(new_modules))
                    else {
                        continue;
                    };
                    // 移動元の経路をほかのファイルも置く (旧い版の cfg の分岐、新しい版で置き換えたファイル)、
                    // 移動先の経路をほかのファイルも置くなら、経路の項目が移動元と移動先の項目と同じとは言えない。
                    if !(self.only(true, root, old_module, old_path)
                        && self.only(false, root, old_module, old_path)
                        && self.only(true, root, new_module, new_path)
                        && self.only(false, root, new_module, new_path))
                    {
                        continue;
                    }
                    let old_placed = expanded(old, old_clauses);
                    let new_placed = expanded(new, new_clauses);
                    // どちらかで公開の経路に置かれなければ、外から使える条件は再公開が決めるので比べない。
                    if old_placed.is_empty() || new_placed.is_empty() {
                        continue;
                    }
                    let old_refs: Vec<&RustDeclarationContext> = old_placed.iter().collect();
                    let new_refs: Vec<&RustDeclarationContext> = new_placed.iter().collect();
                    let change = compare(&old_refs, &new_refs);
                    if let AvailabilityChange::Changed {
                        lost: lost_here,
                        gained: gained_here,
                    } = change
                        && ((lost_here && lost.is_none()) || (gained_here && gained.is_none()))
                    {
                        let shown = describe(&old_refs, &new_refs);
                        if lost_here && lost.is_none() {
                            lost = Some(shown.clone());
                        }
                        if gained_here && gained.is_none() {
                            gained = Some(shown);
                        }
                    }
                    placed = combine(placed, change);
                }
                // 今までの比較が見つけた向きは今までの表示のまま、置き場を足して初めて見つかった向きだけ置き場の
                // 条件を含めて表示する。
                let (plain_lost, plain_gained) = match plain {
                    AvailabilityChange::Changed { lost, gained } => (lost, gained),
                    AvailabilityChange::Same | AvailabilityChange::Unverified => (false, false),
                };
                Decision {
                    change: merge(plain, placed),
                    lost: lost.filter(|_| !plain_lost),
                    gained: gained.filter(|_| !plain_gained),
                }
            }
            // lib crate の module のファイルでない側がある (bin の module・どこにも置かれないファイル): lib の
            // 公開の項目から出入りする変化は、module の比べ直し (#314) とファイルごとの削除・追加が扱うので、
            // 置き場の条件では比べない。
            (Side::Outside, _) | (_, Side::Outside) => Decision::plain(plain),
            // 移動元と移動先の置き場の条件が違い得るが、決められない (今までの比較が見つけた変化は残す)。
            _ => Decision::plain(merge(plain, AvailabilityChange::Unverified)),
        }
    }

    /// 旧新で同じ crate の同じ唯一の公開 module にだけ置かれ、展開による別の置き場もないと確かめられる場合。
    fn same_placement(
        &self,
        path: &str,
        old: &[&RustDeclarationContext],
        new: &[&RustDeclarationContext],
    ) -> Option<Decision> {
        if old.iter().chain(new).any(|context| {
            !context.inline_public
                || !context.owner.is_empty()
                || !context.container_header.is_empty()
                || context.macro_attributes
                || context.availability.is_none()
        }) {
            return None;
        }
        let (old_index, new_index) = (self.index(true), self.index(false));
        if old_index.unknown || new_index.unknown || old_index.uncertain || new_index.uncertain {
            return None;
        }
        let old_crates = old_index.files.get(path)?;
        let new_crates = new_index.files.get(path)?;
        if old_crates.len() != 1 || new_crates.len() != 1 {
            return None;
        }
        let (root, old_modules) = old_crates.first_key_value()?;
        let new_modules = new_crates.get(root)?;
        if old_modules.len() != 1 || new_modules.len() != 1 {
            return None;
        }
        let (module, old_clauses) = old_modules.first_key_value()?;
        let new_clauses = new_modules.get(module)?;
        if old_clauses.is_empty()
            || new_clauses.is_empty()
            || old_clauses
                .iter()
                .chain(new_clauses)
                .any(|clause| !clause.public || clause.attributes.is_none())
            || !self.only(true, root, module, path)
            || !self.only(false, root, module, path)
        {
            return None;
        }
        let old_placed = expanded(old, old_clauses);
        let new_placed = expanded(new, new_clauses);
        let old_refs: Vec<_> = old_placed.iter().collect();
        let new_refs: Vec<_> = new_placed.iter().collect();
        let change = compare(&old_refs, &new_refs);
        if change == AvailabilityChange::Unverified {
            return None;
        }
        let shown = describe(&old_refs, &new_refs);
        Some(Decision {
            change,
            lost: Some(shown.clone()),
            gained: Some(shown),
        })
    }
}

/// 公開の節のある module の経路がちょうど 1 つなら、その経路と節 (ほかの経路は外から使える項目を作らない)。
fn single(modules: &Modules) -> Option<(&[String], &[Clause])> {
    let mut public = modules
        .iter()
        .filter(|(_, clauses)| clauses.iter().any(|clause| clause.public));
    let (module, clauses) = public.next()?;
    public
        .next()
        .is_none()
        .then_some((module.as_slice(), clauses.as_slice()))
}

/// 宣言 `side` を、置き場の節 `clauses` のうち宣言が公開の経路に置かれる節 (経路の `mod` の宣言と、宣言を囲む
/// inline module が全て `pub`) ごとに広げた文脈 (節の属性を宣言の条件の前に足す。#305 の `Entry::expanded` と同じ)。
/// impl・trait の項目は広げない (外から使えるかは対象の型・trait が決める)。
fn expanded(side: &[&RustDeclarationContext], clauses: &[Clause]) -> Vec<RustDeclarationContext> {
    let mut placed = Vec::new();
    for context in side {
        if !context.inline_public || !context.owner.is_empty() {
            continue;
        }
        for clause in clauses.iter().filter(|clause| clause.public) {
            let mut placed_context = (*context).clone();
            placed_context.availability = clause
                .attributes
                .as_ref()
                .zip(context.availability.as_ref())
                .map(|(chain, own)| chain.iter().chain(own).cloned().collect());
            let mut raw = clause.raw.clone();
            raw.push("|".to_owned());
            raw.extend(context.availability_raw.iter().cloned());
            placed_context.availability_raw = raw;
            placed.push(placed_context);
        }
    }
    placed
}

/// 今までの比較 `plain` と置き場を足した比較 `placed` を合わせる (どちらかが報告すれば報告する)。今までの比較が
/// 比べられなかったものは、そのまま比べられなかったものにする。
fn merge(plain: AvailabilityChange, placed: AvailabilityChange) -> AvailabilityChange {
    match plain {
        AvailabilityChange::Unverified => AvailabilityChange::Unverified,
        _ => combine(plain, placed),
    }
}

/// 2 つの比較の結果を合わせる (見つけた変化を合わせる)。片方が変化を見つければ、もう片方が比べられなくても
/// 見つけた変化を残す (比べられなかったものにすると、旧い版で外から使えない項目の増える設定などを落とす)。
fn combine(left: AvailabilityChange, right: AvailabilityChange) -> AvailabilityChange {
    use AvailabilityChange::{Changed, Same, Unverified};
    match (left, right) {
        (Same, other) | (other, Same) => other,
        (
            Changed {
                lost: left_lost,
                gained: left_gained,
            },
            Changed { lost, gained },
        ) => Changed {
            lost: left_lost || lost,
            gained: left_gained || gained,
        },
        (changed @ Changed { .. }, Unverified) | (Unverified, changed @ Changed { .. }) => changed,
        (Unverified, Unverified) => Unverified,
    }
}
