//! 型依存で契約が変わった公開宣言を、通常の影響解析へ引き渡す。

use crate::models::review::{ApiChanges, ApiSymbolChange};
use std::collections::{BTreeMap, BTreeSet};

use super::{
    DetectionInputs, PreparedDiffFile,
    ts_signature::{DependencyChanges, DependencyStatus},
};

pub(super) fn by_path<'a>(
    inputs: &DetectionInputs<'a>,
) -> BTreeMap<&'a str, &'a DependencyChanges> {
    inputs
        .diff_files
        .iter()
        .zip(inputs.prepared)
        .filter_map(|(file, prepared)| match prepared {
            PreparedDiffFile::Modified {
                type_dependencies, ..
            } => Some((file.new_path.as_str(), type_dependencies.as_ref())),
            _ => None,
        })
        .collect()
}

pub(super) fn collect<'a>(
    by_path: &BTreeMap<&str, &DependencyChanges>,
    modified: impl Iterator<Item = (&'a String, &'a String)>,
) -> BTreeMap<String, BTreeSet<String>> {
    let mut symbols: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (file, name) in modified {
        if by_path.get(file.as_str()).and_then(|types| types.get(name))
            == Some(&DependencyStatus::Changed)
        {
            symbols
                .entry(file.clone())
                .or_default()
                .insert(name.clone());
        }
    }
    symbols
}

/// 型依存の一覧は blocking の変更と、戻り値の全利用を確認した変更からだけ作る。
/// 同じ名前で別の変更も記録された場合は、一覧から戻り値の証明を取り出せない。
pub(in crate::commands) fn closed_return_declarations(
    api: &ApiChanges,
) -> BTreeMap<(&str, &str), &ApiSymbolChange> {
    let mut declarations = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for change in &api.modified_closed_in_diff {
        if !api
            .type_dependency_symbols
            .get(&change.file)
            .is_some_and(|names| names.contains(&change.name))
        {
            continue;
        }
        let key = (change.file.as_str(), change.name.as_str());
        if seen.insert(key) && change.kind == "function" {
            declarations.insert(key, change);
        } else {
            declarations.remove(&key);
        }
    }
    if !declarations.is_empty() {
        for key in api
            .modified
            .iter()
            .map(|change| (change.file.as_str(), change.name.as_str()))
            .chain(
                api.compatible_modified
                    .iter()
                    .map(|change| (change.file.as_str(), change.name.as_str())),
            )
        {
            declarations.remove(&key);
        }
    }
    declarations
}
