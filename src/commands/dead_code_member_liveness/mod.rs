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

enum DuplicateSetResult {
    /// 一意推定が成立し、owner 別の (production, test) カウントを保持する。
    Counted(HashMap<String, (usize, usize)>),
    /// 推定不能。呼び出し側で従来のスキップへフォールバックする。
    Ambiguous,
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
