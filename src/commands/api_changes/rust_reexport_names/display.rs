//! 名前の有る設定の表示 (Issue #306・#318)。
//!
//! 名前が有る設定の集まりを、cfg の原子の積の和 (`cfg(feature = "x")`・`cfg(not(unix))` の並びの OR) に
//! まとめる。起こり得ない設定 (1 つの値しか取らない鍵の組み合わせなど) はどちらでもよいものとして、
//! 有る設定ごとに、無い設定を含まない範囲で原子を外していく (最小とは限らないが、正しい)。

use super::super::rust_cfg_predicate::Atoms;

/// 原子の積の和にまとめる設定の数の上限。
const MAX_SHOWN_CONFIGS: usize = 4096;

/// 設定 `configs` (起こり得るもの全て) のうち `present` が真のものを表す、原子の積の和。全ての設定なら
/// `[[]]`、どの設定でもなければ `[]`。
pub(super) fn shown(atoms: &Atoms, configs: &[u64], present: &[bool]) -> Vec<Vec<String>> {
    // 設定が多ければ (原子の積の和をまとめる手間が設定の数の 2 乗で増える)、まとめずに省く。
    if configs.len() > MAX_SHOWN_CONFIGS {
        return vec![vec!["…".to_owned()]];
    }
    let count = atoms.len();
    let all_bits: u64 = if count >= 64 {
        u64::MAX
    } else {
        (1u64 << count) - 1
    };
    let absent: Vec<u64> = configs
        .iter()
        .zip(present)
        .filter(|(_, present)| !**present)
        .map(|(config, _)| *config)
        .collect();
    let mut covered = vec![false; configs.len()];
    let mut terms: Vec<(u64, u64)> = Vec::new();
    for (index, config) in configs.iter().enumerate() {
        if !present[index] || covered[index] {
            continue;
        }
        let mut mask = all_bits;
        for bit in 0..count {
            let candidate = mask & !(1u64 << bit);
            let value = config & candidate;
            if absent.iter().all(|other| other & candidate != value) {
                mask = candidate;
            }
        }
        let value = config & mask;
        for (other_index, other) in configs.iter().enumerate() {
            if other & mask == value {
                covered[other_index] = true;
            }
        }
        terms.push((mask, value));
    }
    terms
        .into_iter()
        .map(|(mask, value)| {
            (0..count)
                .filter(|bit| mask & (1u64 << bit) != 0)
                .map(|bit| {
                    let text = atoms.text(bit);
                    if value & (1u64 << bit) != 0 {
                        format!("cfg({text})")
                    } else {
                        format!("cfg(not({text}))")
                    }
                })
                .collect()
        })
        .collect()
}
