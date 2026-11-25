//! 型依存で契約が変わった公開宣言を、通常の影響解析へ引き渡す。

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
