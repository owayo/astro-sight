//! 型依存で契約が変わった公開宣言を、通常の影響解析へ引き渡す。

use std::collections::{BTreeMap, BTreeSet};

use crate::models::review::ApiSymbolChange;

use super::{DetectionInputs, PreparedDiffFile, ts_signature::DependencyStatus};

pub(super) fn collect(
    inputs: &DetectionInputs<'_>,
    modified: &[ApiSymbolChange],
) -> BTreeMap<String, BTreeSet<String>> {
    let by_path: BTreeMap<_, _> = inputs
        .diff_files
        .iter()
        .zip(inputs.prepared)
        .filter_map(|(file, prepared)| match prepared {
            PreparedDiffFile::Modified {
                type_dependencies, ..
            } => Some((file.new_path.as_str(), type_dependencies)),
            _ => None,
        })
        .collect();
    let mut symbols: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for change in modified {
        if by_path
            .get(change.file.as_str())
            .and_then(|types| types.get(&change.name))
            == Some(&DependencyStatus::Changed)
        {
            symbols
                .entry(change.file.clone())
                .or_default()
                .insert(change.name.clone());
        }
    }
    symbols
}
