//! 別のファイルへ移した Rust の項目の、移動元と移動先の宣言の組み分けと有る設定の比較。

use super::*;

/// 移動元・移動先のファイルの公開の項目と宣言 (読めなければ None)。
type MovedFacts = Option<(ExportedSymbols, Vec<RustDeclarationContext>)>;

/// 宣言と、その (名前, 種類, signature)。
type MovedDeclarations<'a> = Vec<(&'a (String, String, String), &'a RustDeclarationContext)>;

/// 囲む impl の見出しを比べる構文木 (読めなければ None)。
type MovedTree = Option<std::rc::Rc<super::rust_declarations::ParsedSource>>;

/// 宣言の、そろえた impl の対象の型と signature。
type MovedKey = (Vec<String>, String);

/// 移動元 (base) と移動先 (作業ツリー) のファイルの事実。ファイルごとに 1 度だけ読み、読めなかった
/// ことも覚えて読み直さない。
#[derive(Default)]
struct MovedFileFacts {
    old: std::collections::HashMap<String, MovedFacts>,
    new: std::collections::HashMap<String, MovedFacts>,
}

impl MovedFileFacts {
    /// 移動元と移動先のファイルの事実を (まだなら) 読み、同じ名前・種類・inline module の宣言
    /// (`#[cfg]` の分岐を含む) を旧新それぞれ返す。どちらかに無ければ None。
    fn sides(
        &mut self,
        inputs: &DetectionInputs<'_>,
        symbol: &crate::models::review::MovedSymbol,
    ) -> Option<(MovedDeclarations<'_>, MovedDeclarations<'_>)> {
        let facts = |path: &str, source: Option<Vec<u8>>| -> MovedFacts {
            let (symbols, declarations) =
                extract_deleted_exported_facts(path, &source?, inputs.strip_internal)?;
            Some((symbols, declarations?))
        };
        self.old
            .entry(symbol.from.clone())
            .or_insert_with(|| facts(&symbol.from, inputs.base_blobs.read(&symbol.from)));
        self.new.entry(symbol.to.clone()).or_insert_with(|| {
            facts(
                &symbol.to,
                load_new_source(inputs.dir, &symbol.to).map(|source| source.to_vec()),
            )
        });
        let old =
            select_moved_declarations(&self.old[&symbol.from], symbol, &symbol.from_inline_module);
        let new =
            select_moved_declarations(&self.new[&symbol.to], symbol, &symbol.to_inline_module);
        Some((old?, new?))
    }
}

/// 同じ名前・種類・inline module の宣言 (`#[cfg]` の分岐を含む) と、その signature。無ければ None。
fn select_moved_declarations<'a>(
    facts: &'a MovedFacts,
    symbol: &crate::models::review::MovedSymbol,
    inline_module: &Option<String>,
) -> Option<MovedDeclarations<'a>> {
    let (symbols, declarations) = facts.as_ref()?;
    let selected: MovedDeclarations<'a> = symbols
        .iter()
        .zip(declarations)
        .filter(|((name, kind, _), declaration)| {
            *name == symbol.name
                && *kind == symbol.kind
                && inline_module_of(declaration) == *inline_module
        })
        .collect();
    (!selected.is_empty()).then_some(selected)
}

/// 囲む impl の見出しを比べる構文木 (旧新それぞれファイルごとに 1 度だけ作り、作れなかったことも
/// 覚える) と、(移動元, 移動先) ごとの見出しの比較 (並べ替えの探索の結果を、同じファイルの組の移動で
/// 共有する。Issue #307)。
#[derive(Default)]
struct MovedImplTrees {
    old: std::collections::HashMap<String, MovedTree>,
    new: std::collections::HashMap<String, MovedTree>,
    headers: std::collections::HashMap<(String, String), super::rust_declarations::ImplHeaders>,
}

impl MovedImplTrees {
    fn old_tree(&mut self, inputs: &DetectionInputs<'_>, path: &str) -> MovedTree {
        self.old
            .entry(path.to_owned())
            .or_insert_with(|| {
                super::rust_declarations::ParsedSource::parse(inputs.base_blobs.read(path)?)
            })
            .clone()
    }

    fn new_tree(&mut self, inputs: &DetectionInputs<'_>, path: &str) -> MovedTree {
        self.new
            .entry(path.to_owned())
            .or_insert_with(|| {
                super::rust_declarations::ParsedSource::parse(
                    load_new_source(inputs.dir, path)?.to_vec(),
                )
            })
            .clone()
    }
}

/// 移動した項目の、比べる旧新の宣言の組 (添字)。`news` が空の組は、条件の付いた (か読めない) 宣言が
/// あってどれと比べるか決められなかったもので、有る設定を比べられなかった変更にする。
struct MovedGroup {
    olds: Vec<usize>,
    news: Vec<usize>,
}

/// 移動した項目の旧新の宣言の組み分け。
enum MovedDeclarationPairing {
    Groups(Vec<MovedGroup>),
    /// どの宣言どうしを比べるか決められない (移動先で同じ型の宣言が同じ設定に並ぶようになった、
    /// 移動とともに 1 つの impl を複数の対象の型の impl に分けたかまとめた)。
    Ambiguous,
}

/// 別のファイルへ移した Rust の項目 (`moved`) の、移動元 (base) と移動先 (作業ツリー) の宣言 (同じ
/// 場所の `#[cfg]` の分岐をまとめて) の有る設定を比べる (Issue #291)。
pub(crate) fn compare_moved_availability(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
    moved: &[crate::models::review::MovedSymbol],
) {
    let rust = |path: &str| {
        crate::language::LangId::from_path(camino::Utf8Path::new(path)).ok()
            == Some(crate::language::LangId::Rust)
    };
    let mut facts = MovedFileFacts::default();
    let mut trees = MovedImplTrees::default();
    // `#[cfg]` の分岐ごとに移動が並ぶ (signature ごとに 1 つ) ので、同じ項目は 1 度だけ比べる。
    let mut compared = std::collections::HashSet::new();
    for symbol in moved {
        if !rust(&symbol.from) || !rust(&symbol.to) {
            continue;
        }
        if !compared.insert((
            &symbol.from,
            &symbol.to,
            &symbol.name,
            &symbol.kind,
            &symbol.from_inline_module,
            &symbol.to_inline_module,
        )) {
            continue;
        }
        let Some((old_side, new_side)) = facts.sides(inputs, symbol) else {
            continue;
        };
        // impl の対象の型は、型の名前の前の経路を除き、impl の型の引数を位置の名前にそろえる
        // (`impl<T> S<T, u16>` と `impl<V> crate::S<V, u16>` を同じ型にする。Issue #307)。
        let has_owner = |side: &MovedDeclarations<'_>| {
            side.iter()
                .any(|(_, declaration)| !declaration.owner.is_empty())
        };
        let old_tree = if has_owner(&old_side) {
            trees.old_tree(inputs, &symbol.from)
        } else {
            None
        };
        let new_tree = if has_owner(&new_side) {
            trees.new_tree(inputs, &symbol.to)
        } else {
            None
        };
        let old_owners = moved_owners(&old_side, old_tree.as_deref());
        let new_owners = moved_owners(&new_side, new_tree.as_deref());
        match pair_moved_declarations(&old_side, &old_owners, &new_side, &new_owners) {
            MovedDeclarationPairing::Ambiguous => {
                let change = ApiSymbolChange {
                    inline_module: symbol.to_inline_module.clone(),
                    ..ApiSymbolChange::new(
                        symbol.name.clone(),
                        symbol.kind.clone(),
                        symbol.to.clone(),
                        Some(old_side[0].0.2.clone()),
                        Some(new_side[0].0.2.clone()),
                    )
                };
                record_unverified_modification(
                    inputs,
                    state,
                    change,
                    ApiModificationUnverified {
                        reason: ApiModificationUnverifiedReason::RustDeclarationPairingAmbiguous,
                        first_unproven: None,
                    },
                );
            }
            MovedDeclarationPairing::Groups(groups) => {
                for group in &groups {
                    let old_declarations: Vec<&RustDeclarationContext> =
                        group.olds.iter().map(|&index| old_side[index].1).collect();
                    let new_declarations: Vec<&RustDeclarationContext> =
                        group.news.iter().map(|&index| new_side[index].1).collect();
                    let old_signature = &old_side[group.olds[0]].0.2;
                    let new_signature = group.news.first().map(|&index| &new_side[index].0.2);
                    record_moved_impl_header(
                        inputs,
                        state,
                        &mut trees,
                        symbol,
                        (&old_declarations, &new_declarations),
                        (old_signature, new_signature.unwrap_or(&new_side[0].0.2)),
                    );
                    record_moved_availability(
                        inputs,
                        state,
                        symbol,
                        (&old_declarations, &new_declarations),
                        (old_signature, new_signature),
                    );
                }
            }
        }
    }
}

/// 宣言を (impl の対象の型, signature) ごとに組にする。同じ名前の別の型のメソッド (`S<u8>` と
/// `S<u16>`) や、`#[cfg]` の分岐の間で signature を入れ替えた移動を混ぜない。同じ組が両方に無い
/// 宣言は、型の書き方を変えた移動 (`crate::S<u8>` と `super::S<u8>`) として signature だけで組に
/// する。それでも一意に決まらず、条件の付いた (か読めない) 宣言があれば、比べられなかったものと
/// する。片方にしかない signature の宣言 (移動先に元からある別のメソッドなど) は比べない。
/// `old_owners` / `new_owners` は宣言と同じ順の、そろえた impl の対象の型。
fn pair_moved_declarations(
    old_side: &MovedDeclarations<'_>,
    old_owners: &[Vec<String>],
    new_side: &MovedDeclarations<'_>,
    new_owners: &[Vec<String>],
) -> MovedDeclarationPairing {
    let keyed = |side: &MovedDeclarations<'_>, owners: &[Vec<String>]| {
        let mut keyed: std::collections::BTreeMap<MovedKey, Vec<usize>> =
            std::collections::BTreeMap::new();
        for (index, ((symbol, _), owner)) in side.iter().zip(owners).enumerate() {
            keyed
                .entry((owner.clone(), symbol.2.clone()))
                .or_default()
                .push(index);
        }
        keyed
    };
    let (old_keyed, new_keyed) = (keyed(old_side, old_owners), keyed(new_side, new_owners));
    // 移動先で同じ型の宣言が同じ設定に並び (移動元では並ばなかった)、その設定でビルドできない
    // なら、組を決められない変更として blocking にする。
    let duplicated = new_keyed.keys().map(|(owner, _)| owner).any(|owner| {
        let new_owner = declarations_of_owner(new_side, new_owners, owner);
        let old_owner = declarations_of_owner(old_side, old_owners, owner);
        new_owner.len() > 1
            && !exclusive_declarations(new_owner)
            && exclusive_declarations(old_owner)
    });
    if duplicated {
        return MovedDeclarationPairing::Ambiguous;
    }
    let mut groups = Vec::new();
    // signature ごとの、同じ組の無かった旧新の鍵。
    let mut unmatched: std::collections::BTreeMap<String, (Vec<&MovedKey>, Vec<&MovedKey>)> =
        std::collections::BTreeMap::new();
    for (key, olds) in &old_keyed {
        match new_keyed.get(key) {
            Some(news) => groups.push(MovedGroup {
                olds: olds.clone(),
                news: news.clone(),
            }),
            None => unmatched.entry(key.1.clone()).or_default().0.push(key),
        }
    }
    for key in new_keyed.keys().filter(|key| !old_keyed.contains_key(*key)) {
        if let Some((_, news)) = unmatched.get_mut(&key.1) {
            news.push(key);
        }
    }
    let conditional = |declaration: &RustDeclarationContext| {
        declaration
            .availability
            .as_ref()
            .is_none_or(|attributes| !attributes.is_empty())
    };
    for (old_keys, new_keys) in unmatched.into_values() {
        if new_keys.is_empty() {
            continue;
        }
        let olds: Vec<usize> = old_keys
            .iter()
            .flat_map(|key| old_keyed[*key].iter().copied())
            .collect();
        let news: Vec<usize> = new_keys
            .iter()
            .flat_map(|key| new_keyed[*key].iter().copied())
            .collect();
        if old_keys.len() == 1 && new_keys.len() == 1 {
            groups.push(MovedGroup { olds, news });
        } else if olds.iter().any(|&index| conditional(old_side[index].1))
            || news.iter().any(|&index| conditional(new_side[index].1))
        {
            groups.push(MovedGroup {
                olds,
                news: Vec::new(),
            });
        } else if olds
            .iter()
            .any(|&index| !old_side[index].1.owner.is_empty())
        {
            // 移動とともに 1 つの impl を複数の対象の型の impl に分けたか、まとめた (Issue #307)。
            // どの宣言どうしを比べるか決められない。
            return MovedDeclarationPairing::Ambiguous;
        }
    }
    MovedDeclarationPairing::Groups(groups)
}

/// `side` のうち、そろえた対象の型 (`owners`、同じ順) が `owner` の宣言。
fn declarations_of_owner<'a>(
    side: &[(&'a (String, String, String), &'a RustDeclarationContext)],
    owners: &[Vec<String>],
    owner: &[String],
) -> Vec<&'a RustDeclarationContext> {
    side.iter()
        .zip(owners)
        .filter(|(_, key)| key.as_slice() == owner)
        .map(|((_, declaration), _)| *declaration)
        .collect()
}

/// 移動とともに囲む inherent impl の見出しを変えたなら、それも比べる (Issue #307)。片方が 1 つなら、
/// それと他方のどれとも比べる (`#[cfg]` の分岐に分けた・まとめた)。両方が複数なら、有る条件の等しい
/// 分岐どうしを比べ、1 対 1 に組めなければ、どれどうしを比べるか決められないので壊す。見出しの変わった
/// 組のうち最も悪い変化 (同じなら最初のもの) を、見出しを添えた signature で記録する。
fn record_moved_impl_header(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
    trees: &mut MovedImplTrees,
    symbol: &crate::models::review::MovedSymbol,
    (old_declarations, new_declarations): (&[&RustDeclarationContext], &[&RustDeclarationContext]),
    (old_sig, new_sig): (&String, &String),
) {
    use super::rust_type_api::ShapeChange;
    let differs = old_declarations.iter().any(|old| {
        new_declarations
            .iter()
            .any(|new| old.container_header != new.container_header)
    });
    if !differs {
        return;
    }
    let old = trees.old_tree(inputs, &symbol.from);
    let new = trees.new_tree(inputs, &symbol.to);
    let pairs: Option<Vec<(&RustDeclarationContext, &RustDeclarationContext)>> =
        if old_declarations.len() == 1 || new_declarations.len() == 1 {
            Some(
                old_declarations
                    .iter()
                    .flat_map(|old| new_declarations.iter().map(move |new| (*old, *new)))
                    .collect(),
            )
        } else {
            paired_by_availability(old_declarations, new_declarations)
        };
    let header = match (old, new, pairs) {
        (Some(old), Some(new), Some(pairs)) => {
            let headers = trees
                .headers
                .entry((symbol.from.clone(), symbol.to.clone()))
                .or_insert_with(|| super::rust_declarations::ImplHeaders::of(old, new));
            let mut found: Option<(ShapeChange, String, String)> = None;
            for (old_declaration, new_declaration) in pairs {
                let change = headers
                    .change(old_declaration.start, new_declaration.start)
                    .map_or(
                        (ShapeChange::Unchanged, String::new(), String::new()),
                        |change| {
                            // 関連定数の値の型の引数の対応が変わったなら、値の変更を確かめられない
                            // ので壊す。
                            let header = if change.value_changed {
                                ShapeChange::Breaking
                            } else {
                                change.change
                            };
                            (header, change.old_text, change.new_text)
                        },
                    );
                if found.as_ref().is_none_or(|(worst, _, _)| change.0 > *worst) {
                    found = Some(change);
                }
            }
            found
        }
        _ => Some((ShapeChange::Breaking, String::new(), String::new())),
    };
    let Some((header, old_header, new_header)) = header else {
        return;
    };
    if header == ShapeChange::Unchanged {
        return;
    }
    let decision = if header == ShapeChange::Compatible && old_sig == new_sig {
        SignatureDecision::Compatible(CompatibleApiModification {
            change_evidence: None,
            inline_module: symbol.to_inline_module.clone(),
            name: symbol.name.clone(),
            kind: symbol.kind.clone(),
            file: symbol.to.clone(),
            old_signature: Some(old_sig.clone()),
            new_signature: Some(new_sig.clone()),
            reason: "rust_impl_header_compatible".to_owned(),
        })
    } else {
        SignatureDecision::Blocking(None)
    };
    let shown = |header: String, signature: &String| {
        if header.is_empty() {
            signature.clone()
        } else {
            format!("{header} {{ {signature} }}")
        }
    };
    let change = ApiSymbolChange {
        inline_module: symbol.to_inline_module.clone(),
        ..ApiSymbolChange::new(
            symbol.name.clone(),
            symbol.kind.clone(),
            symbol.to.clone(),
            Some(shown(old_header, old_sig)),
            Some(shown(new_header, new_sig)),
        )
    };
    record_signature_decision(inputs, state, change, decision);
}

/// 移動した項目の有る設定を比べて記録する。組を決められなかったとき (`new_declarations` が空) は、
/// 比べられなかったものとする。組を決めたら、移動元の旧い置き場と移動先の新しい置き場の条件も足して
/// 比べる (Issue #316)。`new_sig` は比べた新しい宣言の signature (組を決められなければ None)。
fn record_moved_availability(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
    symbol: &crate::models::review::MovedSymbol,
    (old_declarations, new_declarations): (&[&RustDeclarationContext], &[&RustDeclarationContext]),
    (old_sig, new_sig): (&String, Option<&String>),
) {
    let decision = if new_declarations.is_empty() {
        super::rust_placement_availability::Decision::plain(
            super::rust_availability::AvailabilityChange::Unverified,
        )
    } else {
        let plain = super::rust_availability::compare(old_declarations, new_declarations);
        inputs.placement_availability.decide(
            &symbol.from,
            old_declarations,
            &symbol.to,
            new_declarations,
            plain,
        )
    };
    let change = decision.change;
    if change == super::rust_availability::AvailabilityChange::Same {
        return;
    }
    let (availability, gained_availability) = decision
        .displays(|| super::rust_availability::describe(old_declarations, new_declarations));
    let item = AvailabilityItem {
        candidate: ApiSymbolCandidate {
            inline_module: symbol.to_inline_module.clone(),
            name: symbol.name.clone(),
            kind: symbol.kind.clone(),
            file: symbol.to.clone(),
            signature: new_sig.unwrap_or(old_sig).clone(),
        },
        old_signature: old_sig.clone(),
        availability,
        gained_availability,
        removable: true,
    };
    record_availability_change(inputs, state, change, item, |_| {
        (
            true,
            !is_used_in_diff_paths(
                inputs.ref_index,
                inputs.dir,
                &symbol.name,
                &symbol.to,
                inputs.diff_new_paths,
            ),
        )
    });
}

/// 宣言ごとの、impl の対象の型を移動の組に使う形にそろえた字句 ([`ParsedSource::normalized_owner`])。
/// impl の外の宣言は空。構文木を読めなければ、字面から型の名前の前の経路を除いたもの。
///
/// [`ParsedSource::normalized_owner`]: super::rust_declarations::ParsedSource::normalized_owner
fn moved_owners(
    side: &[(&(String, String, String), &RustDeclarationContext)],
    parsed: Option<&super::rust_declarations::ParsedSource>,
) -> Vec<Vec<String>> {
    side.iter()
        .map(|(_, declaration)| {
            if declaration.owner.is_empty() {
                return Vec::new();
            }
            parsed
                .and_then(|parsed| parsed.normalized_owner(declaration.start))
                .unwrap_or_else(|| {
                    super::rust_declarations::last_segment(declaration.owner.clone())
                })
        })
        .collect()
}

/// 両方が複数の宣言 (`#[cfg]` の分岐) を、有る条件がどの設定でも同じく成り立つものどうしの 1 対 1 の組に
/// する (`#[cfg]` の書き方の違いは同じとみなす)。数が違うか、有る条件を読めないか、同じものが 1 つに決まら
/// なければ None。
fn paired_by_availability<'d>(
    olds: &[&'d RustDeclarationContext],
    news: &[&'d RustDeclarationContext],
) -> Option<Vec<(&'d RustDeclarationContext, &'d RustDeclarationContext)>> {
    if olds.len() != news.len() {
        return None;
    }
    let mut used = vec![false; news.len()];
    let mut pairs = Vec::with_capacity(olds.len());
    for old in olds {
        let mut matching = news
            .iter()
            .enumerate()
            .filter(|(_, new)| super::rust_declarations::same_availability(old, new));
        let (Some((at, new)), None) = (matching.next(), matching.next()) else {
            return None;
        };
        if std::mem::replace(&mut used[at], true) {
            return None;
        }
        pairs.push((*old, *new));
    }
    Some(pairs)
}
