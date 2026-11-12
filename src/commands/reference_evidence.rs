use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use crate::models::reference::{RefKind, SymbolReference};
use crate::models::reference_evidence::*;
use crate::models::review::{ApiSymbolChange, DeadSymbol};
use crate::models::truncation::TruncationInfo;

const SITE_LIMIT: usize = 32;
const SCOPE_FILE_LIMIT: usize = 32;
const METADATA_LIMIT: u64 = 1024 * 1024;

fn relative(root: &Path, path: &Path) -> String {
    crate::git_support::normalize_workspace_separators(
        &path.strip_prefix(root).unwrap_or(path).to_string_lossy(),
    )
}

fn test_rules(file: &str) -> Vec<String> {
    let path = Path::new(file);
    let mut rules = Vec::new();
    if path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(crate::engine::impact::test_context::is_test_file_path)
    {
        rules.push("test_filename_convention".to_owned());
    }
    for part in path.components() {
        if let Some(part) = part.as_os_str().to_str()
            && super::dead_code::TEST_DIRECTORY_SEGMENTS.contains(&part)
        {
            rules.push(format!("test_directory:{part}"));
        }
    }
    rules.sort();
    rules.dedup();
    rules
}

fn record(observations: &mut ReferenceObservations, file: &str, line: usize, column: usize) {
    let test_rules = test_rules(file);
    let test = super::dead_code::is_test_path(Path::new(file));
    observations.total += 1;
    if test {
        observations.test_total += 1;
    } else {
        observations.production_total += 1;
    }
    let site = ReferenceEvidenceSite {
        file: file.to_owned(),
        line,
        column,
        test,
        test_rules,
    };
    // 走査順によらず辞書順の先頭だけを残し、件数は全件を数える。
    if observations.sites.len() < SITE_LIMIT {
        observations.sites.push(site);
        observations.sites.sort();
    } else if observations.sites.last().is_some_and(|last| site < *last) {
        observations.sites.pop();
        observations.sites.push(site);
        observations.sites.sort();
    }
    observations.shown = observations.sites.len();
    observations.omitted = observations.total - observations.shown;
}

fn scope(
    root: &Path,
    files: &[PathBuf],
    selection: &'static str,
    generated: bool,
    complete: bool,
    diagnostics: Vec<TruncationInfo>,
) -> ReferenceScanScope {
    let mut paths: Vec<_> = files.iter().map(|file| relative(root, file)).collect();
    paths.sort();
    paths.dedup();
    let total_files = paths.len();
    paths.truncate(SCOPE_FILE_LIMIT);
    ReferenceScanScope {
        root: ".",
        selection,
        total_files,
        omitted_files: total_files - paths.len(),
        files: paths,
        complete,
        generated_included: generated,
        diagnostics,
    }
}

/// 分類済み集合を変えず、同じ参照走査集合から位置だけを追加する。追加の構文解析は逐次にする。
pub(crate) fn attach_dead(
    dir: &str,
    files: &[PathBuf],
    dead: &mut [DeadSymbol],
    test_only: &mut [DeadSymbol],
    unverified: &mut [DeadSymbol],
    diagnostics: &[TruncationInfo],
) {
    if dead.is_empty() && test_only.is_empty() && unverified.is_empty() {
        return;
    }
    let Ok(root) = std::fs::canonicalize(dir) else {
        return;
    };
    let mut languages = HashMap::new();
    let mut notices = diagnostics.to_vec();
    let mut names = Vec::new();
    for symbol in dead.iter().chain(test_only.iter()).chain(unverified.iter()) {
        let lang = languages
            .entry(symbol.file.clone())
            .or_insert_with(|| crate::engine::refs::detect_source_lang(&root.join(&symbol.file)));
        if let Some(lang) = lang {
            names.push(super::dead_code::normalized_bare_name(*lang, &symbol.name));
        } else {
            notices.push(TruncationInfo::source_analysis_failed(
                &symbol.file,
                "reference evidence candidate language could not be resolved".to_owned(),
            ));
        }
    }
    names.sort();
    names.dedup();
    let mut observations = vec![ReferenceObservations::default(); names.len()];
    let ac = aho_corasick::AhoCorasickBuilder::new()
        .ascii_case_insensitive(true)
        .build(&names);
    if let Ok(ac) = ac {
        let mut paths = files.to_vec();
        paths.sort();
        paths.dedup();
        for path in paths {
            let file = relative(&root, &path);
            let Some(path) = camino::Utf8Path::from_path(&path) else {
                notices.push(TruncationInfo::source_analysis_failed(
                    &file,
                    "evidence path is not UTF-8".to_owned(),
                ));
                continue;
            };
            let mut collect = |index: usize, line: usize, column: usize, is_def: bool| {
                if !is_def && let Some(observations) = observations.get_mut(index) {
                    record(observations, &file, line, column);
                }
            };
            if !matches!(
                crate::engine::refs::visit_ref_positions_in_file(&names, &ac, path, &mut collect,),
                Ok(true)
            ) {
                notices.push(TruncationInfo::source_analysis_failed(
                    &file,
                    "reference evidence was unreadable, unavailable, or had parse errors"
                        .to_owned(),
                ));
            }
        }
    } else {
        notices.push(TruncationInfo::source_analysis_failed(
            ".",
            "reference evidence name index could not be built".to_owned(),
        ));
    }
    let scan = scope(
        &root,
        files,
        "dead_code_reference_scan",
        true,
        notices.is_empty(),
        notices,
    );
    let mut metadata = MetadataCache::default();
    for (symbols, classification) in [
        (dead, ReferenceClassification::NoReferencesInScope),
        (test_only, ReferenceClassification::TestReferencesOnly),
        (unverified, ReferenceClassification::Unverified),
    ] {
        for symbol in symbols {
            let references = languages
                .get(&symbol.file)
                .copied()
                .flatten()
                .and_then(|lang| {
                    names
                        .binary_search(&super::dead_code::normalized_bare_name(lang, &symbol.name))
                        .ok()
                })
                .map(|index| observations[index].clone())
                .unwrap_or_default();
            symbol.reference_evidence = Some(ReferenceEvidence {
                classification: if scan.complete {
                    classification
                } else {
                    ReferenceClassification::Unverified
                },
                attribution: "non_definition_name_match_with_count_filters",
                scope: scan.clone(),
                references,
                external_entry: metadata.evidence(&root, &symbol.file, &symbol.name, &symbol.kind),
            });
        }
    }
}

/// `no_callers` は直接呼び出しを解決できなかった事実で、名前の出現が無いという意味ではない。
pub(crate) fn attach_api<'a>(
    dir: &str,
    changes: &mut [ApiSymbolChange],
    lookup: impl Fn(&str) -> Option<&'a [SymbolReference]>,
    complete: impl Fn(&str) -> bool,
) {
    if !changes
        .iter()
        .any(|change| change.no_resolved_internal_callers)
    {
        return;
    }
    let Ok(root) = std::fs::canonicalize(dir) else {
        return;
    };
    let collection = crate::engine::refs::collect_files_scan(&root, None, Default::default()).ok();
    let files = collection
        .as_ref()
        .map(|collection| collection.files.as_slice())
        .unwrap_or_default();
    let notices = collection
        .as_ref()
        .map(|collection| collection.unanalyzable_truncations(&root))
        .unwrap_or_default();
    let mut metadata = MetadataCache::default();
    for change in changes
        .iter_mut()
        .filter(|change| change.no_resolved_internal_callers)
    {
        let name = super::api_changes::bare_name(&change.name);
        let mut references = ReferenceObservations::default();
        if let Some(found) = lookup(name) {
            for reference in found
                .iter()
                .filter(|reference| reference.kind != Some(RefKind::Definition))
            {
                record(
                    &mut references,
                    &reference.path,
                    reference.line,
                    reference.column,
                );
            }
        }
        let mut diagnostics = notices.clone();
        let complete = collection.is_some() && notices.is_empty() && complete(name);
        if !complete {
            diagnostics.push(TruncationInfo::source_analysis_failed(
                ".",
                "reference-index input completeness was not proven".to_owned(),
            ));
        }
        change.reference_evidence = Some(ReferenceEvidence {
            classification: ReferenceClassification::NoResolvedDirectCallers,
            attribution: "non_definition_name_match",
            scope: scope(
                &root,
                files,
                "api_reference_index",
                false,
                complete,
                diagnostics,
            ),
            references,
            external_entry: metadata.evidence(&root, &change.file, &change.name, &change.kind),
        });
    }
}

/// index の写しが無い縮退時は、作業ツリーの観測を staged の証拠として返さない。
pub(crate) fn clear_staged_fallback(
    result: &mut crate::models::review::ReviewResult,
    side: crate::models::impact::DiffNewSide,
    snapshot_available: bool,
) {
    if snapshot_available || !matches!(side, crate::models::impact::DiffNewSide::Index) {
        return;
    }
    result
        .unverified_symbols
        .append(&mut result.external_entry_symbols);
    let api = &mut result.api_changes;
    for change in api
        .modified
        .iter_mut()
        .chain(api.modified_closed_in_diff.iter_mut())
        .chain(api.const_value_changes.iter_mut())
        .chain(
            api.type_annotation_changes
                .iter_mut()
                .map(|change| &mut change.change),
        )
        .chain(
            api.callback_body_changes
                .iter_mut()
                .map(|change| &mut change.change),
        )
    {
        change.reference_evidence = None;
    }
    for symbol in result
        .dead_symbols
        .iter_mut()
        .chain(result.test_only_symbols.iter_mut())
        .chain(result.unverified_symbols.iter_mut())
    {
        symbol.reference_evidence = None;
    }
}

#[derive(serde::Deserialize)]
struct Pyproject {
    project: Option<Project>,
}
#[derive(serde::Deserialize)]
struct Project {
    scripts: Option<BTreeMap<String, toml::Spanned<String>>>,
}

type ScriptRegistrations = Option<Vec<(String, String, usize)>>;

#[derive(Default)]
struct MetadataCache {
    files: HashMap<PathBuf, ScriptRegistrations>,
    function_bindings: HashMap<PathBuf, Option<BTreeSet<String>>>,
}

impl MetadataCache {
    fn module_function_binding(&mut self, root: &Path, file: &str, name: &str) -> bool {
        let Some(path) = root
            .join(file)
            .canonicalize()
            .ok()
            .filter(|path| path.starts_with(root))
        else {
            return false;
        };
        self.function_bindings
            .entry(path.clone())
            .or_insert_with(|| {
                let path = camino::Utf8Path::from_path(&path)?;
                let source = crate::engine::parser::read_file(path).ok()?;
                let (tree, lang) = crate::engine::parser::parse_file(path, &source).ok()?;
                let module = tree.root_node();
                if lang != crate::language::LangId::Python || module.has_error() {
                    return None;
                }
                let mut names = BTreeMap::<String, usize>::new();
                let mut cursor = module.walk();
                for statement in module.named_children(&mut cursor) {
                    match statement.kind() {
                        "function_definition" => {
                            // 関数定義時に評価される式の副作用による再束縛も証明外にする。
                            for header in [
                                statement.child_by_field_name("parameters"),
                                statement.child_by_field_name("return_type"),
                            ]
                            .into_iter()
                            .flatten()
                            {
                                let mut stack = vec![header];
                                while let Some(node) = stack.pop() {
                                    if node.is_named()
                                        && !matches!(
                                            node.kind(),
                                            "parameters"
                                                | "typed_parameter"
                                                | "identifier"
                                                | "type"
                                        )
                                    {
                                        return None;
                                    }
                                    let mut header_cursor = node.walk();
                                    stack.extend(node.named_children(&mut header_cursor));
                                }
                            }
                            let name = statement
                                .child_by_field_name("name")?
                                .utf8_text(source.as_bytes())
                                .ok()?;
                            *names.entry(name.to_owned()).or_default() += 1;
                        }
                        "comment" => {}
                        // import・代入・decorator などの実行による再束縛は証明しない。
                        _ => return None,
                    }
                }
                Some(
                    names
                        .into_iter()
                        .filter_map(|(name, count)| (count == 1).then_some(name))
                        .collect(),
                )
            })
            .as_ref()
            .is_some_and(|names| names.contains(name))
    }

    fn evidence(
        &mut self,
        root: &Path,
        file: &str,
        name: &str,
        kind: &str,
    ) -> ExternalEntryEvidence {
        let mut result = ExternalEntryEvidence::default();
        if Path::new(file)
            .extension()
            .and_then(|extension| extension.to_str())
            != Some("py")
            || kind != "function"
        {
            result
                .unproven_reasons
                .push("unsupported_external_entry_kind");
            return result;
        }
        let binding_proven = self.module_function_binding(root, file, name);
        if !binding_proven {
            result
                .unproven_reasons
                .push("module_level_function_binding_not_proven");
        }
        let Some(mut directory) = root.join(file).parent().map(Path::to_path_buf) else {
            return result;
        };
        let mut found_metadata = false;
        while directory.starts_with(root) {
            let metadata = directory.join("pyproject.toml");
            if metadata.exists() {
                found_metadata = true;
                let registrations = self.files.entry(metadata.clone()).or_insert_with(|| {
                    if !metadata.canonicalize().ok()?.starts_with(root) {
                        return None;
                    }
                    let text = crate::engine::bounded_read::read_utf8_file_limited(
                        &metadata,
                        METADATA_LIMIT,
                    )?;
                    let parsed: Pyproject = toml::from_str(&text).ok()?;
                    let lines: Vec<_> = std::iter::once(0)
                        .chain(memchr::memchr_iter(b'\n', text.as_bytes()).map(|offset| offset + 1))
                        .collect();
                    let scripts = parsed
                        .project
                        .and_then(|project| project.scripts)
                        .unwrap_or_default();
                    Some(
                        scripts
                            .into_iter()
                            .map(|(script, target)| {
                                let line = lines
                                    .partition_point(|&offset| offset <= target.span().start)
                                    .saturating_sub(1);
                                (script, target.into_inner(), line)
                            })
                            .collect(),
                    )
                });
                if let Some(registrations) = registrations {
                    for (script, target, line) in registrations {
                        let Some((module, callable)) = target.split_once(':') else {
                            continue;
                        };
                        if callable != name || !valid_path(module) || !valid_path(callable) {
                            continue;
                        }
                        let module = directory.join(module.replace('.', "/"));
                        let possible = [module.with_extension("py"), module.join("__init__.py")];
                        let candidates: Vec<_> = possible
                            .iter()
                            .filter_map(|path| path.canonicalize().ok())
                            .filter(|path| path.starts_with(root))
                            .collect();
                        let actual = root.join(file).canonicalize().ok();
                        if binding_proven
                            && candidates.len() == 1
                            && actual.as_ref() == candidates.first()
                        {
                            result.registrations.push(ExternalEntryRegistration {
                                kind: "pyproject_project_scripts",
                                file: relative(root, &metadata),
                                line: *line,
                                script: script.clone(),
                                target: target.clone(),
                            });
                        }
                    }
                } else {
                    result
                        .unproven_reasons
                        .push("metadata_unreadable_or_over_1_mib_or_invalid");
                }
            }
            if directory == root {
                break;
            }
            let Some(parent) = directory.parent() else {
                break;
            };
            directory = parent.to_path_buf();
        }
        result.registrations.sort();
        result.registrations.dedup();
        if !result.registrations.is_empty() {
            result.status = ExternalEntryStatus::Proven;
        } else {
            result.unproven_reasons.push(if found_metadata {
                "no_exact_project_script_registration_proven"
            } else {
                "project_metadata_missing"
            });
        }
        result.unproven_reasons.sort();
        result.unproven_reasons.dedup();
        result
    }
}

fn valid_path(value: &str) -> bool {
    value.split('.').all(|part| {
        !part.is_empty()
            && part.bytes().enumerate().all(|(index, byte)| {
                byte == b'_' || byte.is_ascii_alphabetic() || (index > 0 && byte.is_ascii_digit())
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_evidence_samples_report_totals_and_keep_deterministic_sites() {
        let mut a = ReferenceObservations::default();
        let mut b = ReferenceObservations::default();
        for line in (0..45).rev() {
            record(&mut a, "tests/test_calc.py", line, 4);
        }
        for line in 0..45 {
            record(&mut b, "tests/test_calc.py", line, 4);
        }
        assert_eq!(a.total, 45);
        assert_eq!(a.test_total, 45);
        assert_eq!(a.production_total, 0);
        assert_eq!(a.shown, 32);
        assert_eq!(a.omitted, 13);
        assert_eq!(a.sites, b.sites);
        assert_eq!(
            a.sites[0].test_rules,
            ["test_directory:tests", "test_filename_convention"]
        );
        assert_eq!(a.sites[31].line, 31);
        assert!(test_rules("production/contest.py").is_empty());
    }

    #[test]
    fn reference_evidence_pyproject_records_all_exact_registrations_without_reclassifying() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(
            root.join("calc.py"),
            "def squared(value): return value * value\n",
        )
        .unwrap();
        std::fs::write(root.join("pyproject.toml"), "[project.scripts]\nalpha = 'calc:squared'\nbeta = 'calc:squared'\nwrong = 'other:squared'\n").unwrap();
        let mut cache = MetadataCache::default();
        let evidence = cache.evidence(&root, "calc.py", "squared", "function");
        assert_eq!(evidence.status, ExternalEntryStatus::Proven);
        assert_eq!(evidence.registrations.len(), 2);
        assert_eq!(evidence.registrations[0].line, 1);
        assert_eq!(evidence.registrations[1].line, 2);
        assert_eq!(evidence.registrations[0].file, "pyproject.toml");
        let other = cache.evidence(&root, "calc.py", "other", "function");
        assert_eq!(other.status, ExternalEntryStatus::Unproven);
        assert!(other.registrations.is_empty());
    }

    #[test]
    fn reference_evidence_error_trees_keep_observations_but_mark_scope_incomplete() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let source = root.join("broken.py");
        std::fs::write(&source, "squared(2)\ndef broken(:\n").unwrap();
        let mut symbols = vec![DeadSymbol {
            name: "squared".to_owned(),
            kind: "function".to_owned(),
            file: "calc.py".to_owned(),
            line: Some(0),
            reference_evidence: None,
        }];
        attach_dead(
            root.to_str().unwrap(),
            &[source],
            &mut symbols,
            &mut [],
            &mut [],
            &[],
        );
        let evidence = symbols[0].reference_evidence.as_ref().unwrap();
        assert_eq!(evidence.references.total, 1);
        assert!(!evidence.scope.complete);
        assert_eq!(evidence.classification, ReferenceClassification::Unverified);
        assert!(!evidence.scope.diagnostics.is_empty());
    }

    #[test]
    fn reference_evidence_missing_invalid_and_oversized_metadata_remain_unproven() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(
            root.join("calc.py"),
            "def squared(value): return value * value\n",
        )
        .unwrap();
        let missing = MetadataCache::default().evidence(&root, "calc.py", "squared", "function");
        assert_eq!(missing.status, ExternalEntryStatus::Unproven);
        assert_eq!(missing.unproven_reasons, ["project_metadata_missing"]);
        std::fs::write(root.join("pyproject.toml"), "[project.scripts\n").unwrap();
        assert_eq!(
            MetadataCache::default()
                .evidence(&root, "calc.py", "squared", "function")
                .status,
            ExternalEntryStatus::Unproven
        );
        std::fs::File::create(root.join("pyproject.toml"))
            .unwrap()
            .set_len(METADATA_LIMIT + 1)
            .unwrap();
        let oversized = MetadataCache::default().evidence(&root, "calc.py", "squared", "function");
        assert_eq!(oversized.status, ExternalEntryStatus::Unproven);
        assert!(
            oversized
                .unproven_reasons
                .contains(&"metadata_unreadable_or_over_1_mib_or_invalid")
        );
    }

    #[test]
    fn reference_evidence_nested_duplicate_and_rebound_callables_remain_unproven() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(
            root.join("pyproject.toml"),
            "[project.scripts]\ndemo = 'calc:entry'\n",
        )
        .unwrap();
        for source in [
            "def outer():\n    def entry(): pass\n",
            "def entry(): pass\ndef entry(): pass\n",
            "def entry(): pass\nentry = 0\n",
            "def entry(): pass\nfrom other import *\n",
            "def entry(): pass\ndel entry\n",
            "def entry(): pass\nglobals()['entry'] = 0\n",
            "def entry(): pass\ndef other(value=globals().__setitem__('entry', 0)): pass\n",
        ] {
            std::fs::write(root.join("calc.py"), source).unwrap();
            let evidence = MetadataCache::default().evidence(&root, "calc.py", "entry", "function");
            assert_eq!(evidence.status, ExternalEntryStatus::Unproven, "{source}");
            assert!(evidence.registrations.is_empty());
        }
        std::fs::write(
            root.join("calc.py"),
            "class Outer:\n    class Inner:\n        pass\n",
        )
        .unwrap();
        std::fs::write(
            root.join("pyproject.toml"),
            "[project.scripts]\ndemo = 'calc:Inner'\n",
        )
        .unwrap();
        assert_eq!(
            MetadataCache::default()
                .evidence(&root, "calc.py", "Inner", "class")
                .status,
            ExternalEntryStatus::Unproven
        );
        std::fs::write(
            root.join("calc.py"),
            "# module-level function\ndef entry(): pass\n",
        )
        .unwrap();
        std::fs::write(
            root.join("pyproject.toml"),
            "[project.scripts]\ndemo = 'calc:entry'\n",
        )
        .unwrap();
        assert_eq!(
            MetadataCache::default()
                .evidence(&root, "calc.py", "entry", "function")
                .status,
            ExternalEntryStatus::Proven
        );
    }

    #[test]
    fn reference_evidence_minified_line_keeps_positions_with_lightweight_sink() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tests.js");
        let prefix = "const noise = '🦀';";
        let source = format!("{prefix}{}", "squared();".repeat(20_000));
        std::fs::write(&path, source).unwrap();
        let names = vec!["squared".to_owned()];
        let ac = aho_corasick::AhoCorasick::new(&names).unwrap();
        let mut observations = ReferenceObservations::default();
        let complete = crate::engine::refs::visit_ref_positions_in_file(
            &names,
            &ac,
            camino::Utf8Path::from_path(&path).unwrap(),
            &mut |index, line, column, is_def| {
                assert_eq!(index, 0);
                if !is_def {
                    record(&mut observations, "tests.js", line, column);
                }
            },
        )
        .unwrap();
        assert!(complete);
        assert_eq!(observations.total, 20_000);
        assert_eq!(observations.shown, 32);
        assert_eq!(observations.omitted, 19_968);
        assert_eq!(observations.sites[0].line, 0);
        assert_eq!(observations.sites[0].column, prefix.len());
        assert_eq!(
            observations.sites[31].column,
            prefix.len() + 31 * "squared();".len()
        );
    }
}
