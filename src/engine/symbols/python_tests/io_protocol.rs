//! io の ABC が定める契約と、既存の Python の束縛・継承解決を使った上書きの判定。

use super::{Collected, Universe};

const IO: u8 = 1;
const RAW: u8 = 2;
const BUFFERED: u8 = 4;
const TEXT: u8 = 8;
const ALL: u8 = IO | RAW | BUFFERED | TEXT;

pub(super) fn base_contracts(name: &str) -> u8 {
    match name {
        "IOBase" => IO,
        "RawIOBase" => IO | RAW,
        "BufferedIOBase" => IO | BUFFERED,
        "TextIOBase" => IO | TEXT,
        _ => 0,
    }
}

fn method_contracts(name: &str) -> u8 {
    match name {
        "fileno" | "seek" | "truncate" | "close" | "closed" | "flush" | "isatty" | "readable"
        | "readline" | "readlines" | "seekable" | "tell" | "writable" | "writelines" | "read"
        | "write" => ALL,
        "readinto" => RAW | BUFFERED,
        "readall" => RAW,
        "read1" | "readinto1" => BUFFERED,
        "detach" => BUFFERED | TEXT,
        "encoding" | "errors" | "newlines" => TEXT,
        _ => 0,
    }
}

pub(crate) fn is_io_protocol_name(name: &str) -> bool {
    method_contracts(name) != 0
}

#[derive(Default)]
struct Class {
    bases: Vec<(usize, bool)>,
    derived: Vec<usize>,
    contracts: u8,
    unknown: bool,
}

/// 基底の契約を派生へ伝え、派生の契約を実装しうる基底のメソッドも保護する。
pub(super) fn collect(universe: &impl Universe, collected: &mut Collected) {
    if !(0..universe.module_count()).any(|module| {
        universe
            .facts(module)
            .classes
            .iter()
            .any(|class| !class.io_members.is_empty())
    }) {
        return;
    }
    let mut ids = Vec::new();
    let mut offsets = Vec::new();
    for module in 0..universe.module_count() {
        offsets.push(ids.len());
        ids.extend((0..universe.facts(module).classes.len()).map(|class| (module, class)));
    }
    let mut classes: Vec<Class> = ids.iter().map(|_| Class::default()).collect();
    for (index, &(module, class)) in ids.iter().enumerate() {
        let facts = universe.facts(module);
        let class = &facts.classes[class];
        classes[index].unknown = facts.has_error;
        for base in &class.bases {
            let found = universe.resolve(module, class.owner, base);
            classes[index].contracts |= found.io_bases;
            classes[index].unknown |= found.incomplete || found.wildcard || found.object;
            // 別名の束縛先が複数ある場合は、io の所有権を断定しない。
            classes[index].unknown |= found.io_bases != 0 && !found.classes.is_empty();
            for ((base_module, base_class), confirmed) in found.classes {
                let parent = offsets[base_module] + base_class;
                classes[index].bases.push((parent, confirmed));
                classes[index].unknown |= !confirmed;
                classes[parent].derived.push(index);
            }
        }
    }
    let mut pending: Vec<usize> = (0..classes.len()).collect();
    while let Some(index) = pending.pop() {
        let (contracts, unknown) = (classes[index].contracts, classes[index].unknown);
        for edge in 0..classes[index].derived.len() {
            let child = classes[index].derived[edge];
            let next = &mut classes[child];
            if next.contracts | contracts != next.contracts || (unknown && !next.unknown) {
                next.contracts |= contracts;
                next.unknown |= unknown;
                pending.push(child);
            }
        }
    }

    // 多重継承の mixin にある実装も、io 派生から呼ばれうる。
    let mut certain: Vec<u8> = classes
        .iter()
        .map(|class| if class.unknown { 0 } else { class.contracts })
        .collect();
    let mut uncertain: Vec<u8> = classes
        .iter()
        .map(|class| if class.unknown { ALL } else { 0 })
        .collect();
    pending.extend(0..classes.len());
    while let Some(index) = pending.pop() {
        for &(base, confirmed) in &classes[index].bases {
            let (live, maybe) = if confirmed {
                (certain[index], uncertain[index])
            } else {
                (0, certain[index] | uncertain[index])
            };
            if certain[base] | live != certain[base] || uncertain[base] | maybe != uncertain[base] {
                certain[base] |= live;
                uncertain[base] |= maybe;
                pending.push(base);
            }
        }
    }
    for (index, &(module, class)) in ids.iter().enumerate() {
        for (name, start) in &universe.facts(module).classes[class].io_members {
            let required = method_contracts(name);
            if certain[index] & required != 0 {
                collected.live.insert((module, *start));
                collected.uncertain.remove(&(module, *start));
            } else if uncertain[index] & required != 0
                && !collected.live.contains(&(module, *start))
            {
                collected.uncertain.insert((module, *start));
            }
        }
    }
}
