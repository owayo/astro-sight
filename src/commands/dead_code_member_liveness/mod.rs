//! TS/JS と PHP の class member 単位の owner-aware liveness 解析。
//!
//! 同名 bare member が複数 owner に存在するとき、静的に一意解決できる参照だけを数え、
//! 推定不能な集合は `Ambiguous` として従来の保守的スキップへ戻す。

mod js_ts;
mod php;

pub(crate) use js_ts::JsTsMemberLiveness;
pub(crate) use php::PhpMemberLiveness;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::engine::refs;

/// duplicate な同名 class member の liveness 判定結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemberStatus {
    /// 一意推定で production 参照がある。
    Live,
    /// production では参照 0、test ファイルのみで参照がある。
    TestOnly,
    /// production / test ともに参照 0。
    Dead,
    /// 推定不能。従来の duplicate-name スキップへフォールバックする。
    Ambiguous,
}

struct MemberCandidate {
    owner: String,
    bare: String,
    file: String,
}

/// duplicate set 1 つ分のファイル横断集計 (ループ反転後の共有アキュムレータ)。
/// `ambiguous` は 1 ファイルでも ambiguous 判定が出たら true (sticky)、
/// `counts` は owner 別 (production, test) 参照数。ambiguous の OR と counts の加算は
/// 可換なので、rayon の fold/reduce 順に依らず結果は決定的。
#[derive(Debug, Default, Clone)]
struct SetAccum {
    ambiguous: bool,
    counts: HashMap<String, (usize, usize)>,
}

impl SetAccum {
    /// 2 worker の局所アキュムレータをマージする (reduce 用)。
    fn merge(&mut self, other: SetAccum) {
        self.ambiguous |= other.ambiguous;
        for (owner, (prod, tst)) in other.counts {
            let entry = self.counts.entry(owner).or_insert((0, 0));
            entry.0 = entry.0.saturating_add(prod);
            entry.1 = entry.1.saturating_add(tst);
        }
    }
}

fn collect_source_files(canonical_dir: &Path, extra_files: &[PathBuf]) -> Option<Vec<PathBuf>> {
    let mut files = refs::collect_files(canonical_dir, None).ok()?;
    refs::merge_extra_files(&mut files, canonical_dir, extra_files);
    Some(files)
}

fn status_from_counts(production: usize, tests: usize) -> MemberStatus {
    if production > 0 {
        MemberStatus::Live
    } else if tests > 0 {
        MemberStatus::TestOnly
    } else {
        MemberStatus::Dead
    }
}

fn is_class_member_kind(kind: &str) -> bool {
    matches!(
        kind,
        "method" | "field" | "property" | "getter" | "setter" | "accessor"
    )
}
