//! Rust の公開の項目の有る設定 (Issue #291)。
//!
//! 項目の API の signature は項目自身の `#[cfg]` を含まないので、`#[cfg]` だけの変更 (項目に `#[cfg]` を
//! 足す・条件を狭める) は signature の比較では見えない。旧新の宣言 (同じ場所の `#[cfg]` の分岐はまとめて)
//! の有る条件 ([`RustDeclarationContext::availability`]) を、`rust_cfg_predicate` の設定 (cfg の原子の
//! 真偽の組み合わせ) ごとに比べ、旧版で有った設定で新版に無いもの (消える設定) と、旧版で無かった
//! 設定で新版に有るもの (増える設定) を求める。同値な書き換え (`all(a, b)` と `all(b, a)`、
//! `#[cfg(a)] #[cfg(b)]` と `#[cfg(all(a, b))]`、同じ有る範囲の分岐の分割とまとめ) は変化にしない。

use super::RustDeclarationContext;
use super::rust_cfg_predicate::{Atoms, Predicate};
use crate::models::review::CfgAvailability;

/// 旧新の有る設定を比べた結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AvailabilityChange {
    /// どの設定でも同じ。
    Same,
    /// 消える設定 (`lost`)・増える設定 (`gained`) がある (少なくとも一方は true)。
    Changed { lost: bool, gained: bool },
    /// 読めない条件があるか、原子が多すぎて比べられなかった。
    Unverified,
}

/// 宣言ごとの有る条件の鍵 (属性の鍵を並べ替えたもの)。読めない属性のある宣言があれば None。
fn availability_keys(side: &[&RustDeclarationContext]) -> Option<Vec<Vec<Vec<String>>>> {
    let mut keys: Vec<Vec<Vec<String>>> = side
        .iter()
        .map(|declaration| {
            declaration.availability.as_ref().map(|attributes| {
                let mut keys: Vec<Vec<String>> = attributes
                    .iter()
                    .map(|attribute| attribute.key.clone())
                    .collect();
                keys.sort();
                keys
            })
        })
        .collect::<Option<_>>()?;
    keys.sort();
    Some(keys)
}

/// 宣言の並び (同じ場所の分岐) の有る条件 (分岐の条件の OR、分岐の中は属性の AND)。
fn side_predicate(atoms: &mut Atoms, keys: &[Vec<Vec<String>>]) -> Predicate {
    Predicate::Any(
        keys.iter()
            .map(|declaration| {
                Predicate::All(declaration.iter().map(|key| atoms.presence(key)).collect())
            })
            .collect(),
    )
}

/// 旧新の宣言 (同じキーの宣言をまとめて) の有る設定を比べる。
pub(crate) fn compare(
    old: &[&RustDeclarationContext],
    new: &[&RustDeclarationContext],
) -> AvailabilityChange {
    let (Some(old_keys), Some(new_keys)) = (availability_keys(old), availability_keys(new)) else {
        // 読めない属性があれば、条件の字面 (宣言ごとの cfg の属性のトークン) が同じときだけ同じとする。
        fn conditions<'a>(side: &[&'a RustDeclarationContext]) -> Vec<&'a Vec<String>> {
            let mut conditions: Vec<&Vec<String>> = side
                .iter()
                .map(|declaration| &declaration.conditions)
                .collect();
            conditions.sort();
            conditions
        }
        return if conditions(old) == conditions(new) {
            AvailabilityChange::Same
        } else {
            AvailabilityChange::Unverified
        };
    };
    // 同じ字面の条件は、原子の数に関わらず同じ。
    if old_keys == new_keys {
        return AvailabilityChange::Same;
    }
    let mut atoms = Atoms::default();
    let old_predicate = side_predicate(&mut atoms, &old_keys);
    let new_predicate = side_predicate(&mut atoms, &new_keys);
    let Some(configs) = atoms.configs() else {
        return AvailabilityChange::Unverified;
    };
    let (mut lost, mut gained) = (false, false);
    for config in configs {
        let (was, is) = (old_predicate.holds(config), new_predicate.holds(config));
        lost |= was && !is;
        gained |= !was && is;
        if lost && gained {
            break;
        }
    }
    if lost || gained {
        AvailabilityChange::Changed { lost, gained }
    } else {
        AvailabilityChange::Same
    }
}

/// 出力に添える旧新の有る条件 (宣言ごとの属性の表示)。読めない属性のある宣言は、cfg の属性の
/// トークンをそのまま並べる。
pub(crate) fn describe(
    old: &[&RustDeclarationContext],
    new: &[&RustDeclarationContext],
) -> CfgAvailability {
    let shown = |side: &[&RustDeclarationContext]| -> Vec<Vec<String>> {
        side.iter()
            .map(|declaration| match &declaration.availability {
                Some(attributes) => attributes
                    .iter()
                    .map(|attribute| attribute.shown.clone())
                    .collect(),
                None => vec![declaration.conditions.join(" ")],
            })
            .collect()
    };
    CfgAvailability {
        old: shown(old),
        new: shown(new),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::api_changes::AvailabilityAttr;

    fn declaration(conditions: &[&str]) -> RustDeclarationContext {
        RustDeclarationContext {
            availability: Some(
                conditions
                    .iter()
                    .map(|text| AvailabilityAttr {
                        key: text.split(' ').map(str::to_owned).collect(),
                        shown: (*text).to_owned(),
                    })
                    .collect(),
            ),
            ..RustDeclarationContext::default()
        }
    }

    fn compare_sides(old: &[&[&str]], new: &[&[&str]]) -> AvailabilityChange {
        let old: Vec<RustDeclarationContext> = old.iter().map(|c| declaration(c)).collect();
        let new: Vec<RustDeclarationContext> = new.iter().map(|c| declaration(c)).collect();
        compare(
            &old.iter().collect::<Vec<_>>(),
            &new.iter().collect::<Vec<_>>(),
        )
    }

    #[test]
    fn availability_follows_cfg_semantics() {
        use AvailabilityChange::*;
        let x = "cfg ( feature = \"x\" )";
        // 条件を足す・狭めるのは消える設定、外す・広げるのは増える設定
        assert_eq!(
            compare_sides(&[&[]], &[&[x]]),
            Changed {
                lost: true,
                gained: false
            }
        );
        assert_eq!(
            compare_sides(&[&[x]], &[&[]]),
            Changed {
                lost: false,
                gained: true
            }
        );
        assert_eq!(
            compare_sides(&[&["cfg ( unix )"]], &[&["cfg ( target_os = \"linux\" )"]]),
            Changed {
                lost: true,
                gained: false
            }
        );
        assert_eq!(
            compare_sides(&[&["cfg ( target_os = \"linux\" )"]], &[&["cfg ( unix )"]]),
            Changed {
                lost: false,
                gained: true
            }
        );
        // 別の条件への付け替えは両方
        assert_eq!(
            compare_sides(&[&["cfg ( a )"]], &[&["cfg ( b )"]]),
            Changed {
                lost: true,
                gained: true
            }
        );
        // 同値な書き換えと、分岐の分割・まとめは変わらない
        assert_eq!(
            compare_sides(&[&["cfg ( all ( a , b ) )"]], &[&["cfg ( all ( b , a ) )"]]),
            Same
        );
        assert_eq!(
            compare_sides(
                &[&["cfg ( a )", "cfg ( b )"]],
                &[&["cfg ( all ( a , b ) )"]]
            ),
            Same
        );
        assert_eq!(
            compare_sides(&[&[]], &[&["cfg ( a )"], &["cfg ( not ( a ) )"]]),
            Same
        );
        assert_eq!(
            compare_sides(
                &[&["cfg ( any ( a , b ) )"]],
                &[&["cfg ( a )"], &["cfg ( all ( not ( a ) , b ) )"]]
            ),
            Same
        );
        // `cfg_attr(P, cfg(Q))` は `not(P) or Q`
        assert_eq!(
            compare_sides(
                &[&["cfg_attr ( p , cfg ( q ) )"]],
                &[&["cfg ( any ( not ( p ) , q ) )"]]
            ),
            Same
        );
        // 分岐の片方を消すのは消える設定
        assert_eq!(
            compare_sides(
                &[&["cfg ( unix )"], &["cfg ( windows )"]],
                &[&["cfg ( unix )"]]
            ),
            Changed {
                lost: true,
                gained: false
            }
        );
        // 読めない条件は、字面が同じときだけ同じ
        let unreadable = || RustDeclarationContext {
            availability: None,
            conditions: vec!["#".to_owned(), "[".to_owned(), "cfg".to_owned()],
            ..RustDeclarationContext::default()
        };
        let (old, new) = (unreadable(), unreadable());
        assert_eq!(compare(&[&old], &[&new]), Same);
        let plain = declaration(&[]);
        assert_eq!(compare(&[&old], &[&plain]), Unverified);
    }
}
