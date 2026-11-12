use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::Result;

use crate::engine::{parser, refs, symbols};
use crate::git_support::normalize_workspace_separators;
use crate::models::metrics::*;

mod graph;
mod resolve;

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
    collection.files.sort();
    let mut truncations = collection.generated_undetermined_truncations(dir);
    truncations.extend(collection.unanalyzable_truncations(dir));
    let paths: BTreeMap<String, usize> = collection
        .files
        .iter()
        .enumerate()
        .map(|(id, path)| (relative(dir, path), id))
        .collect();
    let mut components = Vec::new();
    let mut dependencies = Vec::new();
    let mut diagnostics = Vec::new();
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
            resolve::extract(root, &source, lang, &name, &paths, &mut diagnostics)
        })();
        match analyzed {
            Ok(imports) => dependencies.push(imports),
            Err(error) => {
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
    let mut unresolved = Vec::new();
    for (from, imports) in dependencies.into_iter().enumerate() {
        for import in imports {
            match import.resolve(&paths) {
                Ok(to) if from != to => {
                    edges.insert((from, to));
                }
                Ok(_) => {}
                Err(reason) => unresolved.push(UnresolvedDependency {
                    path: components[from].path.clone(),
                    line: import.line,
                    source: import.source,
                    reason: reason.into(),
                }),
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
    Ok(MetricsResult {
        component_unit: opts.component,
        component_depth: (opts.component == ComponentUnit::Directory)
            .then_some(opts.component_depth),
        dependency_basis: "static_local_imports",
        glob: opts.glob.clone(),
        include_generated: opts.include_generated,
        limitations: vec![
            "Metrics describe the selected files and resolved static imports, not a compiler-verified dependency graph.",
            "Runtime loading, fully-qualified uses without imports, build-system aliases/search paths and conditional compilation are not evaluated.",
            "Unresolved/external imports and diagnostics can make coupling metrics lower bounds; empty cycles do not prove the full project acyclic.",
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
        edges: component_edges
            .into_iter()
            .map(|(from, to)| DependencyEdge {
                from: components[from].path.clone(),
                to: components[to].path.clone(),
            })
            .collect(),
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
            })
            .collect(),
        files,
        unresolved_dependencies: unresolved,
        diagnostics,
        skipped: collection.skipped(dir),
        truncations,
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
