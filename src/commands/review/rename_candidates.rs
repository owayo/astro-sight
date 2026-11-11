//! 改名候補は既存の API 分類から独立して集め、判定の降格には使わない。

use std::collections::BTreeMap;
use std::ops::ControlFlow;

use crate::commands::api_changes::exported::filter_exported_file_facts;
use crate::commands::git_input::GitBlobBatch;
use crate::engine::parser;
use crate::engine::tree_walk::{Children, Walk, walk_preorder};
use crate::language::LangId;
use crate::models::impact::DiffFile;
use crate::models::rename_candidate::*;

const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;
const MAX_NODES: usize = 100_000;
const MAX_CANDIDATES: usize = 1_000;

#[cfg(test)]
mod tests;

#[derive(Default)]
pub(super) struct RenameReport {
    pub candidates: Vec<RenameCandidate>,
    pub diagnostics: Vec<RenameCandidateDiagnostic>,
}

pub(super) fn clear_staged_fallback(
    result: &mut crate::models::review::ReviewResult,
    side: crate::models::impact::DiffNewSide,
    snapshot_available: bool,
) {
    if snapshot_available || side != crate::models::impact::DiffNewSide::Index {
        return;
    }
    result.rename_candidates.clear();
    result.rename_candidate_diagnostics = vec![RenameCandidateDiagnostic {
        file: None,
        side: None,
        reason: RenameDiagnosticReason::SourceUnavailable,
        omitted: None,
    }];
}

struct Declaration {
    location: RenameDeclaration,
    name_key: String,
    lang: String,
    structure: Vec<u8>,
    root: bool,
    reexports: std::rc::Rc<Vec<Reexport>>,
}

type DeclarationIdentity<'a> = (&'a str, &'a str, &'a str, &'a str, usize, usize, &'a [u8]);

fn unchanged_declaration_key(item: &Declaration) -> DeclarationIdentity<'_> {
    (
        &item.lang,
        &item.location.file,
        &item.location.kind,
        &item.name_key,
        item.location.line,
        item.location.column,
        &item.structure,
    )
}

#[derive(Clone)]
struct Reexport {
    old: String,
    new: String,
    location: RenameDeclaration,
}

pub(super) fn collect_rename_candidates(
    dir: &str,
    diff: &[DiffFile],
    blobs: &GitBlobBatch,
) -> RenameReport {
    let mut report = RenameReport::default();
    let mut old = Vec::new();
    let mut new = Vec::new();
    let mut total_bytes = 0;
    let mut files: Vec<_> = diff.iter().collect();
    files.sort_by(|a, b| (&a.old_path, &a.new_path).cmp(&(&b.old_path, &b.new_path)));
    for file in files {
        for (path, side, target) in [
            (&file.old_path, RenameSourceSide::Old, &mut old),
            (&file.new_path, RenameSourceSide::New, &mut new),
        ] {
            if path == "/dev/null" {
                continue;
            }
            if !crate::engine::impact::is_safe_diff_path(path) {
                report.skip(path, side, RenameDiagnosticReason::SourceUnavailable);
                continue;
            }
            let source = match side {
                RenameSourceSide::Old => blobs
                    .read_limited(path, crate::commands::common::MAX_INPUT_SIZE)
                    .map(parser::SourceBuf::Vec),
                RenameSourceSide::New => {
                    let full_path = std::path::Path::new(dir).join(path);
                    camino::Utf8Path::from_path(&full_path)
                        .and_then(|path| parser::read_file(path).ok())
                }
            };
            let Some(source) = source else {
                report.skip(path, side, RenameDiagnosticReason::SourceUnavailable);
                continue;
            };
            if source.len() > MAX_FILE_BYTES {
                report.skip(path, side, RenameDiagnosticReason::SourceSizeLimit);
                continue;
            }
            if total_bytes + source.len() > MAX_TOTAL_BYTES {
                report.skip(path, side, RenameDiagnosticReason::TotalSourceSizeLimit);
                continue;
            }
            total_bytes += source.len();
            collect_declarations(path, &source, side, target, &mut report);
        }
    }
    pair_declarations(old, new, &mut report, MAX_CANDIDATES);
    report
}

impl RenameReport {
    fn skip(&mut self, path: &str, side: RenameSourceSide, reason: RenameDiagnosticReason) {
        self.diagnostics.push(RenameCandidateDiagnostic {
            file: Some(crate::git_support::normalize_workspace_separators(path)),
            side: Some(side),
            reason,
            omitted: None,
        });
    }
}

fn collect_declarations(
    path: &str,
    source: &[u8],
    side: RenameSourceSide,
    declarations: &mut Vec<Declaration>,
    report: &mut RenameReport,
) {
    let Ok(lang) = parser::detect_lang(camino::Utf8Path::new(path), source) else {
        // 文書や設定は候補の対象言語ではない。
        return;
    };
    if lang.is_lexer_only() {
        report.skip(path, side, RenameDiagnosticReason::UnsupportedLanguage);
        return;
    }
    let Ok(tree) = parser::parse_source(source, lang) else {
        report.skip(path, side, RenameDiagnosticReason::ParseError);
        return;
    };
    let root = tree.root_node();
    if root.has_error() {
        report.skip(path, side, RenameDiagnosticReason::ParseError);
        return;
    }
    let mut budget = MAX_NODES;
    if !spend_nodes(root, &mut budget) {
        report.skip(path, side, RenameDiagnosticReason::NodeLimit);
        return;
    }
    let Ok(symbols) = crate::engine::symbols::extract_symbols(root, source, lang) else {
        report.skip(path, side, RenameDiagnosticReason::ParseError);
        return;
    };
    let facts =
        filter_exported_file_facts(&symbols, root, source, lang, true, false, Some(path), false);
    let exported: BTreeMap<_, _> = facts.starts.into_iter().zip(facts.symbols).collect();
    let reexports = std::rc::Rc::new(direct_rust_reexports(root, source, path, lang));
    for symbol in symbols {
        let start = symbol.range.start;
        let Some((name, kind, _)) = exported.get(&(start.line, start.column)) else {
            continue;
        };
        let end = symbol.range.end;
        let Some(node) = root.descendant_for_point_range(
            tree_sitter::Point::new(start.line, start.column),
            tree_sitter::Point::new(end.line, end.column),
        ) else {
            report.skip(path, side, RenameDiagnosticReason::DeclarationUnsupported);
            continue;
        };
        let Some(binding) = node
            .child_by_field_name("name")
            .filter(|binding| source.get(binding.byte_range()) == Some(symbol.name.as_bytes()))
        else {
            report.skip(path, side, RenameDiagnosticReason::DeclarationUnsupported);
            continue;
        };
        let Some(structure) = fingerprint(node, binding, source, &mut budget) else {
            report.skip(path, side, RenameDiagnosticReason::NodeLimit);
            break;
        };
        declarations.push(Declaration {
            location: RenameDeclaration {
                file: crate::git_support::normalize_workspace_separators(path),
                name: name.clone(),
                kind: kind.clone(),
                line: binding.start_position().row,
                column: binding.start_position().column,
            },
            name_key: if lang == LangId::Rust {
                name.split('.')
                    .map(|part| part.strip_prefix("r#").unwrap_or(part))
                    .collect::<Vec<_>>()
                    .join(".")
            } else {
                name.clone()
            },
            lang: lang.to_string(),
            structure,
            root: node.parent().is_some_and(|parent| parent == root)
                && !has_preceding_attribute(node),
            reexports: reexports.clone(),
        });
    }
}

fn spend_nodes(root: tree_sitter::Node<'_>, remaining: &mut usize) -> bool {
    walk_preorder(root, Children::All, |_, _| {
        if *remaining == 0 {
            return ControlFlow::Break(());
        }
        *remaining -= 1;
        ControlFlow::Continue(Walk::Descend)
    })
    .is_continue()
}

fn fingerprint(
    root: tree_sitter::Node<'_>,
    binding: tree_sitter::Node<'_>,
    source: &[u8],
    budget: &mut usize,
) -> Option<Vec<u8>> {
    let mut result = Vec::new();
    let finished: ControlFlow<()> = walk_preorder(root, Children::All, |node, depth| {
        if *budget == 0 {
            return ControlFlow::Break(());
        }
        *budget -= 1;
        if node.is_extra() {
            return ControlFlow::Continue(Walk::Skip);
        }
        result.extend_from_slice(node.kind().as_bytes());
        result.push(0);
        result.extend_from_slice(&(depth as u64).to_le_bytes());
        if node == binding {
            result.push(1);
            return ControlFlow::Continue(Walk::Skip);
        } else if node.child_count() == 0 {
            result.push(2);
            let text = &source[node.byte_range()];
            result.extend_from_slice(&(text.len() as u64).to_le_bytes());
            result.extend_from_slice(text);
        } else {
            result.push(3);
        }
        result.push(0);
        ControlFlow::Continue(Walk::Descend)
    });
    finished.is_continue().then_some(result)
}

fn pair_declarations(
    old: Vec<Declaration>,
    new: Vec<Declaration>,
    report: &mut RenameReport,
    limit: usize,
) {
    // 別ファイルの同名や、位置を変えた同構造の宣言は、同じ宣言が残った証拠にしない。
    let mut pending: BTreeMap<_, Vec<usize>> = BTreeMap::new();
    for (index, item) in new.iter().enumerate().rev() {
        pending
            .entry(unchanged_declaration_key(item))
            .or_default()
            .push(index);
    }
    let mut retained_old = vec![false; old.len()];
    let mut retained_new = vec![false; new.len()];
    for (index, item) in old.iter().enumerate() {
        if let Some(other) = pending
            .get_mut(&unchanged_declaration_key(item))
            .and_then(Vec::pop)
        {
            retained_old[index] = true;
            retained_new[other] = true;
        }
    }
    let removed: Vec<_> = old
        .iter()
        .zip(retained_old)
        .filter_map(|(item, retained)| (!retained).then_some(item))
        .collect();
    let added: Vec<_> = new
        .iter()
        .zip(retained_new)
        .filter_map(|(item, retained)| (!retained).then_some(item))
        .collect();
    let mut groups: BTreeMap<_, (Vec<&Declaration>, Vec<&Declaration>)> = BTreeMap::new();
    for item in removed {
        groups
            .entry((&item.lang, &item.location.kind, &item.structure))
            .or_default()
            .0
            .push(item);
    }
    for item in added {
        groups
            .entry((&item.lang, &item.location.kind, &item.structure))
            .or_default()
            .1
            .push(item);
    }
    let new_counts: BTreeMap<_, usize> = new.iter().fold(BTreeMap::new(), |mut counts, item| {
        *counts
            .entry((
                &item.lang,
                &item.location.kind,
                &item.location.name,
                &item.location.file,
            ))
            .or_default() += 1;
        counts
    });
    let mut total = 0usize;
    for (_, (old, new)) in groups {
        let counts_by_name = |items: &[&Declaration]| {
            items.iter().fold(BTreeMap::new(), |mut counts, item| {
                *counts.entry(item.name_key.clone()).or_insert(0usize) += 1;
                counts
            })
        };
        let old_names = counts_by_name(&old);
        let new_names = counts_by_name(&new);
        total = total.saturating_add(old.iter().fold(0usize, |count, before| {
            count.saturating_add(
                new.len()
                    - new_names
                        .get(before.name_key.as_str())
                        .copied()
                        .unwrap_or(0),
            )
        }));
        for before in &old {
            for after in &new {
                if before.name_key == after.name_key {
                    continue;
                }
                if report.candidates.len() == limit {
                    break;
                }
                let new_candidates_for_old = new.len()
                    - new_names
                        .get(before.name_key.as_str())
                        .copied()
                        .unwrap_or(0);
                let old_candidates_for_new =
                    old.len() - old_names.get(after.name_key.as_str()).copied().unwrap_or(0);
                let evidence: Vec<_> = after
                    .reexports
                    .iter()
                    .filter(|alias| {
                        before.root
                            && after.root
                            && before.location.file == after.location.file
                            && alias.old == before.location.name
                            && alias.new == after.location.name
                            && new_counts.get(&(
                                &after.lang,
                                &after.location.kind,
                                &after.location.name,
                                &after.location.file,
                            )) == Some(&1)
                    })
                    .map(|alias| alias.location.clone())
                    .collect();
                report.candidates.push(RenameCandidate {
                    old: before.location.clone(),
                    new: after.location.clone(),
                    status: RenameCandidateStatus::Candidate,
                    evidence: vec![RenameEvidence::NameIndependentAstEqual],
                    // 同名の移動・再配置という別の対応も、確定できなければ競合として残す。
                    ambiguous: old.len() > 1 || new.len() > 1,
                    new_candidates_for_old,
                    old_candidates_for_new,
                    old_api_presence: OldApiPresence {
                        status: if evidence.is_empty() {
                            OldApiPresenceStatus::Unverified
                        } else {
                            OldApiPresenceStatus::DirectReexportPresent
                        },
                        compatibility: RenameCompatibility::Unverified,
                        evidence,
                    },
                });
            }
        }
    }
    report.candidates.sort_by(|a, b| {
        (
            &a.old.file,
            a.old.line,
            a.old.column,
            &a.old.name,
            &a.new.file,
            a.new.line,
            a.new.column,
            &a.new.name,
        )
            .cmp(&(
                &b.old.file,
                b.old.line,
                b.old.column,
                &b.old.name,
                &b.new.file,
                b.new.line,
                b.new.column,
                &b.new.name,
            ))
    });
    if total > report.candidates.len() {
        report.diagnostics.push(RenameCandidateDiagnostic {
            file: None,
            side: None,
            reason: RenameDiagnosticReason::CandidateLimit,
            omitted: Some(total - report.candidates.len()),
        });
    }
}

fn direct_rust_reexports(
    root: tree_sitter::Node<'_>,
    source: &[u8],
    path: &str,
    lang: LangId,
) -> Vec<Reexport> {
    if lang != LangId::Rust {
        return Vec::new();
    }
    let mut root_cursor = root.walk();
    if root
        .named_children(&mut root_cursor)
        .any(|node| node.kind() == "inner_attribute_item")
    {
        return Vec::new();
    }
    let mut result = Vec::new();
    let mut cursor = root.walk();
    for node in root.named_children(&mut cursor) {
        if node.kind() != "use_declaration" || has_preceding_attribute(node) {
            continue;
        }
        let mut tokens = Vec::new();
        let _: ControlFlow<()> = walk_preorder(node, Children::All, |leaf, _| {
            if leaf.is_extra() {
                return ControlFlow::Continue(Walk::Skip);
            }
            if leaf.child_count() == 0 {
                tokens.push(&source[leaf.byte_range()]);
            }
            ControlFlow::Continue(Walk::Descend)
        });
        let names = match tokens.as_slice() {
            [b"pub", b"use", new, b"as", old, b";"] => Some((*new, *old)),
            [b"pub", b"use", b"self", b"::", new, b"as", old, b";"] => Some((*new, *old)),
            _ => None,
        };
        if let Some((new, old)) = names {
            let (Ok(new), Ok(old)) = (std::str::from_utf8(new), std::str::from_utf8(old)) else {
                continue;
            };
            result.push(Reexport {
                old: old.into(),
                new: new.into(),
                location: RenameDeclaration {
                    file: crate::git_support::normalize_workspace_separators(path),
                    name: old.into(),
                    kind: "reexport".into(),
                    line: node.start_position().row,
                    column: node.start_position().column,
                },
            });
        }
    }
    result
}

fn has_preceding_attribute(node: tree_sitter::Node<'_>) -> bool {
    let mut previous = node.prev_named_sibling();
    while let Some(node) = previous {
        if !node.is_extra() {
            return node.kind() == "attribute_item";
        }
        previous = node.prev_named_sibling();
    }
    false
}
