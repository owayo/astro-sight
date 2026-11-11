use std::collections::{BTreeSet, HashSet};

use crate::models::added_reference_summary::{
    AddedReferenceCount, AddedReferenceLocation, AddedReferenceScope, AddedReferenceSource,
    AddedReferenceSummary,
};
use crate::models::reference::RefKind;
use crate::models::review::{ApiSymbol, MovedSymbol};

use super::{ApiRefIndex, ApiSymbolCandidate, bare_name};

const LOCATION_SAMPLE_LIMIT: usize = 20;

pub(super) fn collect(
    candidates: &[ApiSymbolCandidate],
    added: &[ApiSymbol],
    moved: &[MovedSymbol],
    index: &ApiRefIndex,
    diff_paths: &HashSet<String>,
) -> Vec<AddedReferenceSummary> {
    let mut names = BTreeSet::new();
    for candidate in candidates {
        names.insert((
            candidate.file.clone(),
            candidate.name.clone(),
            candidate.kind.clone(),
            candidate.inline_module.clone(),
        ));
    }
    for symbol in added {
        names.insert((
            symbol.file.clone(),
            symbol.name.clone(),
            symbol.kind.clone(),
            symbol.inline_module.clone(),
        ));
    }
    names
        .into_iter()
        .filter(|(file, name, kind, module)| {
            !moved.iter().any(|moved| {
                moved.to == *file
                    && moved.name == *name
                    && moved.kind == *kind
                    && moved.to_inline_module == *module
            })
        })
        .map(|(file, name, kind, inline_module)| {
            let mut scan = index.added_reference_scan.clone();
            let references = index.refs_for(bare_name(&name));
            if let Some(references) = references {
                scan.unknown_references += references
                    .iter()
                    .filter(|reference| {
                        !matches!(
                            reference.kind,
                            Some(RefKind::Definition | RefKind::Reference)
                        )
                    })
                    .count();
            } else {
                scan.scan_failed = true;
            }
            let complete = index.complete_refs_for(bare_name(&name)).is_some()
                && scan.parse_error_files == 0
                && scan.unknown_references == 0;
            let partition = |inside| {
                let locations = references
                    .into_iter()
                    .flatten()
                    .filter(|reference| reference.kind == Some(RefKind::Reference))
                    .map(|reference| AddedReferenceLocation {
                        path: crate::git_support::normalize_workspace_separators(&reference.path),
                        line: reference.line,
                        column: reference.column,
                    })
                    .filter(move |location| diff_paths.contains(&location.path) == inside);
                count(locations, complete)
            };
            AddedReferenceSummary {
                file: crate::git_support::normalize_workspace_separators(&file),
                name,
                kind,
                inline_module,
                scope: AddedReferenceScope {
                    new_side: AddedReferenceSource::WorkingTree,
                    partition: "diff_files",
                    references: "non_definition_identifier_occurrences",
                    attribution: "name_match",
                    selection: "refs_default_workspace_scan",
                },
                in_diff: partition(true),
                outside_diff: partition(false),
                scan,
            }
        })
        .collect()
}

fn count(
    locations: impl IntoIterator<Item = AddedReferenceLocation>,
    complete: bool,
) -> AddedReferenceCount {
    let mut references: Vec<AddedReferenceLocation> = Vec::new();
    let mut previous = None;
    let mut observed = 0;
    for location in locations {
        // 索引は位置順。同じ位置は一度だけ数え、表示の保持数だけを制限する。
        if previous.as_ref() == Some(&location) {
            continue;
        }
        previous = Some(location.clone());
        observed += 1;
        let position = references.partition_point(|sample| sample < &location);
        if position < LOCATION_SAMPLE_LIMIT {
            references.insert(position, location);
            references.truncate(LOCATION_SAMPLE_LIMIT);
        }
    }
    AddedReferenceCount {
        observed,
        total: complete.then_some(observed),
        complete,
        omitted: observed - references.len(),
        references,
    }
}

pub(crate) fn apply_source(
    summaries: &mut Vec<AddedReferenceSummary>,
    new_side: crate::models::impact::DiffNewSide,
    has_snapshot: bool,
) {
    if new_side == crate::models::impact::DiffNewSide::Index {
        if has_snapshot {
            for summary in summaries {
                summary.scope.new_side = AddedReferenceSource::Index;
            }
        } else {
            // index を読めないとき、既存の fallback の作業ツリーを index の証拠にしない。
            summaries.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn added_reference_summary_samples_keep_exact_counts_and_stable_order() {
        let locations = (0..31).rev().map(|line| AddedReferenceLocation {
            path: "reader.ts".into(),
            line,
            column: 1,
        });
        let result = count(locations, true);
        assert_eq!(result.observed, 31);
        assert_eq!(result.total, Some(31));
        assert_eq!(result.references.len(), LOCATION_SAMPLE_LIMIT);
        assert_eq!(result.omitted, 11);
        assert_eq!(result.references[0].line, 0);
        assert_eq!(result.references[19].line, 19);
        let incomplete = count(result.references, false);
        assert_eq!(incomplete.total, None);
        assert!(!incomplete.complete);
        assert_eq!(incomplete.observed, 20);
    }

    #[test]
    fn added_reference_summary_failed_scan_and_missing_snapshot_keep_unknown() {
        let temporary = tempfile::tempdir().unwrap();
        let index = ApiRefIndex::build(
            temporary.path().join("missing").to_str().unwrap(),
            &HashSet::from(["cube".into()]),
        );
        let mut summaries = collect(
            &[ApiSymbolCandidate {
                name: "cube".into(),
                kind: "function".into(),
                inline_module: None,
                file: "math.ts".into(),
                signature: "cube()".into(),
            }],
            &[],
            &[],
            &index,
            &HashSet::from(["math.ts".into()]),
        );
        assert!(summaries[0].scan.scan_failed);
        assert_eq!(summaries[0].in_diff.total, None);
        assert_eq!(summaries[0].outside_diff.total, None);
        assert!(!summaries[0].in_diff.complete);
        apply_source(
            &mut summaries,
            crate::models::impact::DiffNewSide::WorkingTree,
            false,
        );
        assert_eq!(summaries.len(), 1);
        apply_source(
            &mut summaries,
            crate::models::impact::DiffNewSide::Index,
            true,
        );
        assert!(matches!(
            summaries[0].scope.new_side,
            AddedReferenceSource::Index
        ));
        apply_source(
            &mut summaries,
            crate::models::impact::DiffNewSide::Index,
            false,
        );
        assert!(summaries.is_empty());
    }
}
