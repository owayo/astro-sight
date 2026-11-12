use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::Result;

use crate::engine::{parser, refs, symbols};
use crate::git_support::normalize_workspace_separators;
use crate::models::metrics::*;

pub(crate) mod comparison;
mod graph;
mod js_config;
mod resolve;
mod rust_modules;

pub(crate) fn analyze(
    dir: &Path,
    opts: &MetricsOptions,
    validate_path: impl Fn(&Path) -> Result<()>,
) -> Result<MetricsResult> {
    let mut collection = refs::collect_files_scan(
        dir,
        opts.glob.as_deref(),
        refs::FileScanOptions {
            include_generated: opts.include_generated,
        },
    )?;
    // ファイルとコンポーネントで同じ ID を使えるよう、出力パスの文字列順にそろえる。
    collection
        .files
        .sort_by_cached_key(|path| relative(dir, path));
    let mut truncations = collection.generated_undetermined_truncations(dir);
    truncations.extend(collection.unanalyzable_truncations(dir));
    let paths: BTreeMap<String, usize> = collection
        .files
        .iter()
        .enumerate()
        .map(|(id, path)| (relative(dir, path), id))
        .collect();
    let mut components = Vec::new();
    let resolution = resolve::ResolutionIndex {
        rust: rust_modules::RustModules::build(dir, &paths, &validate_path)?,
        javascript: js_config::JsConfigs::build(dir, &paths, &validate_path)?,
    };
    let mut dependencies = Vec::new();
    let mut diagnostics = resolution.javascript.diagnostics.clone();
    let mut total = ComplexityStats::default();
    // 逐次解析し、ファイルの処理が済んだら Tree を解放する。
    for path in &collection.files {
        let name = relative(dir, path);
        let mut component = FileMetrics {
            path: name.clone(),
            language: "unknown".into(),
            complexity: ComplexityStats::default(),
            functions: Vec::new(),
            fan_in: 0,
            fan_out: 0,
            depends_on: 1,
            used_by: 1,
            instability: 0.0,
        };
        let analyzed = (|| -> Result<_> {
            validate_path(path)?;
            let utf8 = camino::Utf8Path::from_path(path)
                .ok_or_else(|| anyhow::anyhow!("Non-UTF-8 source path"))?;
            let source = parser::read_file(utf8)?;
            if !resolution.rust.matches_source(&name, &source) {
                return Err(crate::error::AstroError::new(
                    crate::error::ErrorCode::ReviewInputChanged,
                    "Source changed while building the metrics module index; rerun metrics",
                )
                .into());
            }
            let lang = parser::detect_lang(utf8, &source)?;
            component.language = lang.to_string();
            if lang.is_lexer_only() {
                diagnostics.push(resolve::diagnostic(
                    &name,
                    None,
                    "unsupported_language",
                    "Complexity and dependencies are unavailable for lexer-only sources".into(),
                ));
                return Ok(Vec::new());
            }
            let tree = parser::parse_source(&source, lang)?;
            let root = tree.root_node();
            if root.has_error() {
                diagnostics.push(resolve::diagnostic(
                    &name,
                    None,
                    "parse_error",
                    "Syntax errors may make complexity and dependencies incomplete".into(),
                ));
            }
            let extracted = symbols::extract_symbols(root, &source, lang)?;
            let mut pending: Vec<_> = extracted.iter().rev().collect();
            while let Some(symbol) = pending.pop() {
                if let Some(value) = symbol.complexity {
                    component.complexity.add(value);
                    total.add(value);
                    component.functions.push(FunctionComplexity {
                        name: symbol.name.clone(),
                        line: symbol.name_line(),
                        container: symbol.container.clone(),
                        complexity: value,
                    });
                }
                pending.extend(symbol.children.iter().rev());
            }
            component.functions.sort_by(|a, b| {
                a.line
                    .cmp(&b.line)
                    .then(a.name.cmp(&b.name))
                    .then(a.container.cmp(&b.container))
                    .then(a.complexity.cmp(&b.complexity))
            });
            resolve::extract(
                root,
                &source,
                lang,
                &name,
                &paths,
                &resolution,
                &mut diagnostics,
            )
        })();
        match analyzed {
            Ok(imports) => dependencies.push(imports),
            Err(error) => {
                if error
                    .downcast_ref::<crate::error::AstroError>()
                    .is_some_and(|e| e.code == crate::error::ErrorCode::ReviewInputChanged)
                {
                    return Err(error);
                }
                diagnostics.push(resolve::diagnostic(
                    &name,
                    None,
                    "analysis_failed",
                    error.to_string(),
                ));
                dependencies.push(Vec::new());
            }
        }
        components.push(component);
    }
    let mut edges = BTreeSet::new();
    let mut evidence: BTreeMap<(usize, usize), BTreeSet<DependencyEvidence>> = BTreeMap::new();
    let mut summary = ResolutionSummary::default();
    let mut external = Vec::new();
    let mut unresolved = Vec::new();
    for (from, imports) in dependencies.into_iter().enumerate() {
        for import in imports {
            summary.static_imports += 1;
            match import.resolve(&paths) {
                Ok(to) if from != to => {
                    summary.resolved_local_imports += 1;
                    edges.insert((from, to));
                    evidence
                        .entry((from, to))
                        .or_default()
                        .insert(DependencyEvidence {
                            path: components[from].path.clone(),
                            line: import.line,
                            source: import.source,
                            target: components[to].path.clone(),
                            kind: import.kind,
                            resolution: import.rule.into(),
                        });
                }
                Ok(_) => {
                    summary.resolved_local_imports += 1;
                }
                Err("external") => {
                    summary.external_imports += 1;
                    external.push(UnresolvedDependency {
                        path: components[from].path.clone(),
                        line: import.line,
                        source: import.source,
                        reason: "declared_external".into(),
                    });
                }
                Err(reason) => {
                    summary.unresolved_imports += 1;
                    summary.ambiguous_imports +=
                        usize::from(matches!(reason, "ambiguous" | "ambiguous_path_mapping"));
                    unresolved.push(UnresolvedDependency {
                        path: components[from].path.clone(),
                        line: import.line,
                        source: import.source,
                        reason: reason.into(),
                    });
                }
            }
        }
    }
    unresolved.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.line.cmp(&b.line))
            .then(a.source.cmp(&b.source))
            .then(a.reason.cmp(&b.reason))
    });
    unresolved.dedup_by(|a, b| {
        a.path == b.path && a.line == b.line && a.source == b.source && a.reason == b.reason
    });
    diagnostics.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.line.cmp(&b.line))
            .then(a.reason.cmp(&b.reason))
            .then(a.message.cmp(&b.message))
    });
    let graph = graph::analyze(components.len(), &edges);
    for (id, component) in components.iter_mut().enumerate() {
        component.fan_in = graph.fan_in[id];
        component.fan_out = graph.fan_out[id];
        component.depends_on = graph.depends_on[id];
        component.used_by = graph.used_by[id];
        let degree = component.fan_in + component.fan_out;
        component.instability = if degree == 0 {
            0.0
        } else {
            component.fan_out as f64 / degree as f64
        };
    }
    let files = components;
    let (mut components, owners) = group_components(&files, opts);
    let component_edges: BTreeSet<_> = edges
        .iter()
        .filter_map(|&(a, b)| (owners[a] != owners[b]).then_some((owners[a], owners[b])))
        .collect();
    let mut grouped_evidence: BTreeMap<(usize, usize), BTreeSet<DependencyEvidence>> =
        BTreeMap::new();
    for (&(a, b), proof) in &evidence {
        if owners[a] != owners[b] {
            grouped_evidence
                .entry((owners[a], owners[b]))
                .or_default()
                .extend(proof.iter().cloned());
        }
    }
    let grouped = if opts.component == ComponentUnit::File {
        graph
    } else {
        graph::analyze(components.len(), &component_edges)
    };
    for (id, component) in components.iter_mut().enumerate() {
        component.fan_in = grouped.fan_in[id];
        component.fan_out = grouped.fan_out[id];
        component.depends_on = grouped.depends_on[id];
        component.used_by = grouped.used_by[id];
        let degree = component.fan_in + component.fan_out;
        component.instability = if degree == 0 {
            0.0
        } else {
            component.fan_out as f64 / degree as f64
        };
    }
    let count = components.len();
    let ccd = grouped.depends_on.iter().map(|&n| n as u64).sum();
    let acd = if count == 0 {
        0.0
    } else {
        ccd as f64 / count as f64
    };
    let pc = if count == 0 { 0.0 } else { acd / count as f64 };
    let edge_output: Vec<_> = component_edges
        .iter()
        .map(|&(a, b)| DependencyEdge {
            from: components[a].path.clone(),
            to: components[b].path.clone(),
            evidence: grouped_evidence[&(a, b)].iter().cloned().collect(),
        })
        .collect();
    let cycle_paths = graph::representative_cycles(count, &grouped.cycles, &component_edges)?;
    let cycle_evidence = grouped
        .cycles
        .iter()
        .zip(cycle_paths)
        .map(|(group, path)| CycleEvidence {
            components: group
                .iter()
                .map(|&id| components[id].path.clone())
                .collect(),
            steps: path
                .into_iter()
                .map(|(a, b)| DependencyEdge {
                    from: components[a].path.clone(),
                    to: components[b].path.clone(),
                    evidence: grouped_evidence[&(a, b)].iter().cloned().collect(),
                })
                .collect(),
        })
        .collect();
    summary.dynamic_imports = diagnostics
        .iter()
        .filter(|d| d.reason == "dynamic_import")
        .count();
    summary.unsupported_files = diagnostics
        .iter()
        .filter(|d| {
            matches!(
                d.reason.as_str(),
                "unsupported_language" | "dependency_resolution_unsupported"
            )
        })
        .map(|d| &d.path)
        .collect::<BTreeSet<_>>()
        .len();
    summary.failed_files = diagnostics
        .iter()
        .filter(|d| matches!(d.reason.as_str(), "analysis_failed" | "parse_error"))
        .map(|d| &d.path)
        .collect::<BTreeSet<_>>()
        .len();
    summary.all_static_imports_resolved =
        summary.unresolved_imports == 0 && diagnostics.is_empty() && truncations.is_empty();
    Ok(MetricsResult {
        component_unit: opts.component,
        component_depth: (opts.component == ComponentUnit::Directory)
            .then_some(opts.component_depth),
        dependency_basis: "static_local_imports",
        glob: opts.glob.clone(),
        include_generated: opts.include_generated,
        limitations: vec![
            "Metrics describe the selected files and resolved static imports, not a compiler-verified dependency graph.",
            "Rust declared modules and selected TypeScript configuration rules are resolved; runtime loading, arbitrary build-system rules, fully-qualified uses without imports and active cfg conditions are not evaluated.",
            "Unresolved imports and diagnostics can omit local edges; declared external packages are counted separately. Empty cycles do not prove the full project acyclic.",
        ],
        complexity: total,
        dependencies: DependencyMetrics {
            component_count: count,
            edge_count: component_edges.len(),
            cyclic_component_count: grouped.cycles.iter().map(Vec::len).sum(),
            ccd,
            acd,
            propagation_cost: pc,
            propagation_cost_percent: pc * 100.0,
        },
        edges: edge_output,
        cycle_evidence_mode: "one_representative_per_scc",
        cycle_evidence,
        resolution_summary: summary,
        external_dependencies: external,
        cycles: grouped
            .cycles
            .into_iter()
            .map(|group| {
                group
                    .into_iter()
                    .map(|id| components[id].path.clone())
                    .collect()
            })
            .collect(),
        components,
        file_edges: edges
            .into_iter()
            .map(|(a, b)| DependencyEdge {
                from: files[a].path.clone(),
                to: files[b].path.clone(),
                evidence: evidence[&(a, b)].iter().cloned().collect(),
            })
            .collect(),
        files,
        unresolved_dependencies: unresolved,
        diagnostics,
        skipped: collection.skipped(dir),
        truncations,
        comparison: None,
    })
}

fn group_components(
    files: &[FileMetrics],
    opts: &MetricsOptions,
) -> (Vec<ComponentMetrics>, Vec<usize>) {
    let names: Vec<String> = files
        .iter()
        .map(|file| {
            if opts.component == ComponentUnit::File {
                return file.path.clone();
            }
            let parent = file.path.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
            let group = parent
                .split('/')
                .take(opts.component_depth)
                .collect::<Vec<_>>()
                .join("/");
            if group.is_empty() { ".".into() } else { group }
        })
        .collect();
    let index: BTreeMap<_, _> = names
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .enumerate()
        .map(|(id, path)| (path, id))
        .collect();
    let mut components: Vec<_> = index
        .keys()
        .map(|path| ComponentMetrics {
            path: path.clone(),
            files: Vec::new(),
            complexity: ComplexityStats::default(),
            fan_in: 0,
            fan_out: 0,
            depends_on: 1,
            used_by: 1,
            instability: 0.0,
        })
        .collect();
    let mut owners = Vec::new();
    for (file, name) in files.iter().zip(names) {
        let id = index[&name];
        owners.push(id);
        let component = &mut components[id];
        component.files.push(file.path.clone());
        for function in &file.functions {
            component.complexity.add(function.complexity);
        }
    }
    (components, owners)
}

fn relative(dir: &Path, path: &Path) -> String {
    normalize_workspace_separators(&path.strip_prefix(dir).unwrap_or(path).to_string_lossy())
}
