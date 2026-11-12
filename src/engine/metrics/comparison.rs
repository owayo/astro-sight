use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;

use crate::models::metrics::*;

pub(crate) fn attach(
    current: &mut MetricsResult,
    baseline: MetricsResult,
    commit: String,
) -> Result<()> {
    let before = &baseline.dependencies;
    let after = &current.dependencies;
    let delta = MetricsDelta {
        component_count: difference(before.component_count as u64, after.component_count as u64)?,
        edge_count: difference(before.edge_count as u64, after.edge_count as u64)?,
        cyclic_component_count: difference(
            before.cyclic_component_count as u64,
            after.cyclic_component_count as u64,
        )?,
        ccd: difference(before.ccd, after.ccd)?,
        acd: after.acd - before.acd,
        propagation_cost: after.propagation_cost - before.propagation_cost,
        complexity_count: difference(
            baseline.complexity.count as u64,
            current.complexity.count as u64,
        )?,
        complexity_total: difference(
            baseline.complexity.total as u64,
            current.complexity.total as u64,
        )?,
        complexity_average: current.complexity.average - baseline.complexity.average,
        complexity_max: difference(
            baseline.complexity.max as u64,
            current.complexity.max as u64,
        )?,
    };
    let old: BTreeMap<_, _> = baseline.components.iter().map(|c| (&c.path, c)).collect();
    let new: BTreeMap<_, _> = current.components.iter().map(|c| (&c.path, c)).collect();
    let added_components = new
        .keys()
        .filter(|path| !old.contains_key(*path))
        .map(|p| (*p).clone())
        .collect();
    let removed_components = old
        .keys()
        .filter(|path| !new.contains_key(*path))
        .map(|p| (*p).clone())
        .collect();
    let changed_components = new
        .iter()
        .filter_map(|(&path, &after)| {
            let &before = old.get(path)?;
            (before != after).then(|| ComponentChange {
                path: path.clone(),
                before: before.clone(),
                after: after.clone(),
            })
        })
        .collect();
    let old_edges: BTreeSet<_> = baseline.edges.iter().map(|e| (&e.from, &e.to)).collect();
    let new_edges: BTreeSet<_> = current.edges.iter().map(|e| (&e.from, &e.to)).collect();
    let added_edges = current
        .edges
        .iter()
        .filter(|e| !old_edges.contains(&(&e.from, &e.to)))
        .cloned()
        .collect();
    let removed_edges = baseline
        .edges
        .iter()
        .filter(|e| !new_edges.contains(&(&e.from, &e.to)))
        .cloned()
        .collect();
    let old_cycles: BTreeSet<_> = baseline.cycles.iter().collect();
    let new_cycles: BTreeSet<_> = current.cycles.iter().collect();
    let added_cycle_groups = new_cycles
        .difference(&old_cycles)
        .map(|group| (*group).clone())
        .collect();
    let removed_cycle_groups = old_cycles
        .difference(&new_cycles)
        .map(|group| (*group).clone())
        .collect();
    let normalization_changed = before.component_count != after.component_count;
    let coverage_changed = baseline.resolution_summary.all_static_imports_resolved
        != current.resolution_summary.all_static_imports_resolved
        || baseline.resolution_summary.unsupported_files
            != current.resolution_summary.unsupported_files
        || baseline.resolution_summary.failed_files != current.resolution_summary.failed_files
        || baseline.unresolved_dependencies != current.unresolved_dependencies
        || baseline.truncations != current.truncations
        || baseline
            .diagnostics
            .iter()
            .map(|d| (&d.path, d.line, &d.reason))
            .collect::<Vec<_>>()
            != current
                .diagnostics
                .iter()
                .map(|d| (&d.path, d.line, &d.reason))
                .collect::<Vec<_>>();
    let assessment = if baseline.resolution_summary.all_static_imports_resolved
        && current.resolution_summary.all_static_imports_resolved
    {
        "observed_static_graphs"
    } else {
        "partial_observed_graphs"
    };
    let function_changes = compare_functions(&baseline.files, &current.files);
    current.comparison = Some(MetricsComparison {
        base_commit: commit,
        target: "working_tree",
        baseline: Box::new(baseline),
        delta,
        normalization_changed,
        coverage_changed,
        assessment,
        added_components,
        removed_components,
        changed_components,
        function_changes,
        added_edges,
        removed_edges,
        added_cycle_groups,
        removed_cycle_groups,
    });
    Ok(())
}

fn compare_functions(before: &[FileMetrics], after: &[FileMetrics]) -> Vec<FunctionChange> {
    type Key = (String, Option<String>, String);
    let group = |files: &[FileMetrics]| {
        let mut groups: BTreeMap<Key, Vec<FunctionComplexity>> = BTreeMap::new();
        for file in files {
            for function in &file.functions {
                groups
                    .entry((
                        file.path.clone(),
                        function.container.clone(),
                        function.name.clone(),
                    ))
                    .or_default()
                    .push(function.clone());
            }
        }
        groups
    };
    let old = group(before);
    let new = group(after);
    let keys: BTreeSet<_> = old.keys().chain(new.keys()).cloned().collect();
    let mut changes = Vec::new();
    for key in keys {
        let before = old.get(&key).cloned().unwrap_or_default();
        let after = new.get(&key).cloned().unwrap_or_default();
        let pairing = match (before.as_slice(), after.as_slice()) {
            ([before], [after]) if before.complexity == after.complexity => continue,
            ([], _) => "added_observation",
            (_, []) => "removed_observation",
            ([_], [_]) => "unique_name_and_container",
            _ if before == after => continue,
            _ => "ambiguous_name_and_container",
        };
        changes.push(FunctionChange {
            path: key.0,
            container: key.1,
            name: key.2,
            pairing,
            before,
            after,
        });
    }
    changes
}

fn difference(before: u64, after: u64) -> Result<i64> {
    Ok(i64::try_from(i128::from(after) - i128::from(before))?)
}
