//! シンボルの範囲から所属を決める。抽出順と同順位の選択規則は維持する。

use std::cmp::Reverse;

use crate::models::location::{Point, Range};
use crate::models::symbol::{Symbol, SymbolKind};

type Rank = (usize, usize);

fn position(point: Point) -> (usize, usize) {
    (point.line, point.column)
}

fn rank(index: usize, range: Range) -> Rank {
    // 従来は行数だけで比較し、同じ行数なら抽出順で最初の候補を選んでいた。
    (range.end.line.saturating_sub(range.start.line), index)
}

/// 内包する候補のうち行数が最小のものを選び、確定済みの所属は上書きしない。
pub(super) fn assign_enclosing_containers(symbols: &mut [Symbol]) {
    let mut containers: Vec<_> = symbols
        .iter()
        .enumerate()
        .filter(|(_, symbol)| {
            matches!(
                symbol.kind,
                SymbolKind::Class
                    | SymbolKind::Struct
                    | SymbolKind::Trait
                    | SymbolKind::Interface
                    | SymbolKind::Enum
                    | SymbolKind::Type
            )
        })
        .map(|(index, symbol)| (index, symbol.range))
        .collect();
    if containers.is_empty() {
        return;
    }
    let mut targets: Vec<_> = symbols
        .iter()
        .enumerate()
        .filter(|(_, symbol)| {
            matches!(symbol.kind, SymbolKind::Function | SymbolKind::Method)
                && symbol.container.is_none()
        })
        .map(|(index, _)| index)
        .collect();

    // 小さいファイルでは索引の構築より直接比較の方が安い。
    if containers.len() <= 32 || targets.len() <= 16 {
        for index in targets {
            let target = symbols[index].range;
            let best = containers
                .iter()
                .filter(|(_, range)| {
                    position(range.start) <= position(target.start)
                        && position(range.end) >= position(target.end)
                })
                .map(|&(index, range)| rank(index, range))
                .min();
            if let Some((_, owner)) = best {
                symbols[index].container = Some(symbols[owner].name.clone());
            }
        }
        return;
    }

    // 終点の降順で候補を追加する。この時点で索引にある候補は終点条件を満たす。
    // 始点順の prefix 最小値を引けば、交差・重複範囲でも元の最良候補を得られる。
    containers.sort_unstable_by_key(|&(index, range)| (position(range.start), index));
    let mut by_end: Vec<_> = (0..containers.len()).collect();
    by_end.sort_unstable_by_key(|&index| Reverse(position(containers[index].1.end)));
    targets.sort_unstable_by_key(|&index| Reverse(position(symbols[index].range.end)));
    let mut active = PrefixMinimum::new(containers.len());
    let mut next = 0;
    for index in targets {
        let target = symbols[index].range;
        while next < by_end.len()
            && position(containers[by_end[next]].1.end) >= position(target.end)
        {
            let slot = by_end[next];
            let (owner, range) = containers[slot];
            active.insert(slot, rank(owner, range));
            next += 1;
        }
        let prefix = containers
            .partition_point(|&(_, range)| position(range.start) <= position(target.start));
        if let Some((_, owner)) = active.minimum(prefix) {
            symbols[index].container = Some(symbols[owner].name.clone());
        }
    }
}

/// 追加だけを行う Fenwick tree。候補の保持量は O(C)、追加・prefix 照会は O(log C)。
struct PrefixMinimum {
    nodes: Vec<Option<Rank>>,
}

impl PrefixMinimum {
    fn new(len: usize) -> Self {
        Self {
            nodes: vec![None; len + 1],
        }
    }

    fn insert(&mut self, slot: usize, rank: Rank) {
        let mut index = slot + 1;
        while index < self.nodes.len() {
            self.nodes[index] = Some(self.nodes[index].map_or(rank, |old| old.min(rank)));
            index += index.isolate_lowest_one();
        }
    }

    fn minimum(&self, mut end: usize) -> Option<Rank> {
        let mut best = None;
        while end > 0 {
            if let Some(rank) = self.nodes[end] {
                best = Some(best.map_or(rank, |old: Rank| old.min(rank)));
            }
            end &= end - 1;
        }
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn symbol(name: &str, kind: SymbolKind, start: (usize, usize), end: (usize, usize)) -> Symbol {
        Symbol {
            name: name.into(),
            kind,
            range: Range {
                start: Point {
                    line: start.0,
                    column: start.1,
                },
                end: Point {
                    line: end.0,
                    column: end.1,
                },
            },
            doc: None,
            complexity: None,
            container: None,
            children: Vec::new(),
            name_range: None,
        }
    }

    /// 変更前の全組合せ照合。列境界と行数だけによる優先順位も独立に照合する。
    fn original_owners(symbols: &[Symbol]) -> Vec<Option<String>> {
        symbols
            .iter()
            .map(|target| {
                if target.container.is_some()
                    || !matches!(target.kind, SymbolKind::Function | SymbolKind::Method)
                {
                    return target.container.clone();
                }
                let mut best = None;
                for (index, candidate) in symbols.iter().enumerate() {
                    if !matches!(
                        candidate.kind,
                        SymbolKind::Class
                            | SymbolKind::Struct
                            | SymbolKind::Trait
                            | SymbolKind::Interface
                            | SymbolKind::Enum
                            | SymbolKind::Type
                    ) {
                        continue;
                    }
                    let outer = candidate.range;
                    let inner = target.range;
                    if outer.start.line > inner.start.line
                        || outer.end.line < inner.end.line
                        || (outer.start.line == inner.start.line
                            && outer.start.column > inner.start.column)
                        || (outer.end.line == inner.end.line && outer.end.column < inner.end.column)
                    {
                        continue;
                    }
                    let size = outer.end.line.saturating_sub(outer.start.line);
                    if best.is_none_or(|(_, previous)| size < previous) {
                        best = Some((index, size));
                    }
                }
                best.map(|(index, _)| symbols[index].name.clone())
            })
            .collect()
    }

    fn assert_matches_original(mut symbols: Vec<Symbol>) {
        let expected = original_owners(&symbols);
        let before: Vec<_> = symbols.iter().map(|symbol| symbol.name.clone()).collect();
        assign_enclosing_containers(&mut symbols);
        assert_eq!(
            symbols
                .iter()
                .map(|symbol| symbol.container.clone())
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(
            symbols
                .iter()
                .map(|symbol| symbol.name.clone())
                .collect::<Vec<_>>(),
            before
        );
    }

    #[test]
    fn container_assignment_preserves_boundaries_ties_and_explicit_owners() {
        for padding in [0, 40] {
            let mut symbols = vec![
                symbol("first", SymbolKind::Class, (0, 0), (0, 90)),
                symbol("inner", SymbolKind::Type, (0, 10), (0, 80)),
                symbol("method", SymbolKind::Method, (0, 10), (0, 80)),
                symbol("left", SymbolKind::Function, (0, 0), (0, 91)),
                symbol("right", SymbolKind::Function, (0, 11), (0, 91)),
                symbol("outer", SymbolKind::Trait, (1, 0), (9, 0)),
                symbol("smaller", SymbolKind::Struct, (2, 0), (5, 0)),
                symbol("nested", SymbolKind::Method, (3, 0), (4, 0)),
                symbol("fixed", SymbolKind::Method, (3, 0), (4, 0)),
            ];
            symbols[8].container = Some("receiver".into());
            for i in 0..padding {
                symbols.push(symbol(
                    &format!("padding{i}"),
                    SymbolKind::Enum,
                    (20 + i, 0),
                    (21 + i, 0),
                ));
                symbols.push(symbol(
                    "target",
                    SymbolKind::Function,
                    (100 + i, 0),
                    (101 + i, 0),
                ));
            }
            let expected = original_owners(&symbols);
            assert_eq!(expected[2].as_deref(), Some("first"));
            assert_eq!(expected[3], None);
            assert_eq!(expected[4], None);
            assert_eq!(expected[7].as_deref(), Some("smaller"));
            assert_eq!(expected[8].as_deref(), Some("receiver"));
            assert_matches_original(symbols);
        }
        assert_matches_original(Vec::new());
        assert_matches_original(vec![symbol("free", SymbolKind::Function, (0, 0), (1, 0))]);
    }

    #[test]
    fn container_assignment_matches_original_for_overlapping_and_reordered_ranges() {
        // 通常の木に加え、custom query が返せる交差・同一・反転範囲も比較する。
        let kinds = [
            SymbolKind::Class,
            SymbolKind::Struct,
            SymbolKind::Trait,
            SymbolKind::Interface,
            SymbolKind::Enum,
            SymbolKind::Type,
            SymbolKind::Method,
            SymbolKind::Function,
            SymbolKind::Variable,
        ];
        let mut state = 1234567_u32;
        let mut next = || {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            state as usize
        };
        let symbols: Vec<_> = (0..1000)
            .map(|index| {
                symbol(
                    &index.to_string(),
                    kinds[index % kinds.len()],
                    (next() % 40, next() % 20),
                    (next() % 40, next() % 20),
                )
            })
            .collect();
        assert_matches_original(symbols.clone());
        assert_matches_original(symbols.into_iter().rev().collect());
    }

    #[test]
    fn container_assignment_keeps_owners_after_parsing_many_declarations() {
        use crate::engine::parser::parse_source;
        use crate::language::LangId;

        for language in [
            LangId::Rust,
            LangId::Python,
            LangId::Typescript,
            LangId::Tsx,
        ] {
            let mut source = String::new();
            for index in 0..64 {
                source.push_str(&match language {
                    LangId::Rust => format!(
                        "struct Box{index};\nimpl Box{index} {{ fn method{index}(&self) {{}} }}\n"
                    ),
                    LangId::Python => {
                        format!("class Box{index}:\n    def method{index}(self):\n        pass\n")
                    }
                    _ => format!(
                        "class Box{index} {{ method{index}(): number {{ return {index}; }} }}\n"
                    ),
                });
            }
            let tree = parse_source(source.as_bytes(), language).unwrap();
            assert!(!tree.root_node().has_error(), "{language:?}");
            let symbols =
                super::super::extract_symbols(tree.root_node(), source.as_bytes(), language)
                    .unwrap();
            for index in 0..64 {
                let name = format!("method{index}");
                let method = symbols.iter().find(|symbol| symbol.name == name).unwrap();
                assert_eq!(
                    method.container.as_deref(),
                    Some(format!("Box{index}").as_str()),
                    "{language:?} {name}"
                );
            }
        }
    }
}
