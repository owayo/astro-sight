//! `impl Trait` と async の戻り値に漏れる auto trait を、本体と宣言の依存から保守的に比べる。
//!
//! 変更された library package の宣言を名前でつなぐ過大近似であり、型や局所束縛の同一性は証明しない。
//! 本体が参照し得る宣言が変われば、同名の候補を含めて未検証の契約変更として残す。公開面と、
//! 両立しない cfg の判定は既存の証明を使う。ファイル数・読み込み量・parse の不足は申告する。

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ops::ControlFlow;

use super::*;
use crate::engine::rust_use_tree::{RustPubUseEdge, expand_rust_use_tree_edges_ast};
use crate::engine::tree_walk::{Children, Walk, any_node, walk_preorder};
use crate::models::review::{ApiModificationUnverified, ApiModificationUnverifiedReason};
use crate::models::truncation::{TruncationInfo, TruncationReason};

const MAX_FILES: usize = 10_000;
const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;
const MAX_BYTES: usize = 64 * 1024 * 1024;
const UNKNOWN_IMPORTS: &str = "@unknown-imports";

type Key = (String, [u8; 32], String, String);
type ChangedNames = BTreeMap<String, Vec<Vec<AvailabilityAttr>>>;

struct Scope {
    depth: usize,
    name: String,
    key: blake3::Hash,
}

struct PublicFunction {
    name: String,
    kind: String,
    signature: String,
    context: RustDeclarationContext,
    body: blake3::Hash,
    references: BTreeSet<String>,
}

struct Declaration {
    fingerprint: blake3::Hash,
    name: String,
    bound_names: BTreeSet<String>,
    references: BTreeSet<String>,
    conditions: Vec<AvailabilityAttr>,
    public: Option<PublicFunction>,
}

#[derive(Default)]
struct Snapshot {
    declarations: BTreeMap<Key, Vec<Declaration>>,
}

fn public_functions(declarations: &[Declaration]) -> Vec<&PublicFunction> {
    declarations
        .iter()
        .filter_map(|item| item.public.as_ref())
        .collect()
}

fn bodies_changed(before: &[&PublicFunction], after: &[&PublicFunction]) -> bool {
    let mut groups: BTreeMap<_, (Vec<_>, Vec<_>)> = BTreeMap::new();
    for function in before {
        groups
            .entry(*function.body.as_bytes())
            .or_default()
            .0
            .push(&function.context);
    }
    for function in after {
        groups
            .entry(*function.body.as_bytes())
            .or_default()
            .1
            .push(&function.context);
    }
    groups.values().any(|(old, new)| {
        super::rust_availability::compare(old, new)
            != super::rust_availability::AvailabilityChange::Same
    })
}

fn import_bound_names(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    path: &str,
    scopes: &[Scope],
) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut edges = Vec::new();
    let expanded = (|| {
        let (_, mut module) = super::rust_public::source_module_path(path)?;
        module.extend(
            scopes
                .iter()
                .filter_map(|scope| scope.name.strip_prefix("mod_item:").map(str::to_owned)),
        );
        let argument = node.child_by_field_name("argument")?;
        expand_rust_use_tree_edges_ast(argument, source, &[], &module, &module, None, &mut edges)?;
        // `_` の trait import は束縛名を作らず、メソッド解決を変え得る。
        (!any_node(argument, Children::Named, |node| {
            node.kind() == "use_as_clause"
                && node
                    .child_by_field_name("alias")
                    .is_some_and(|alias| alias.utf8_text(source) == Ok("_"))
        }))
        .then_some(())
    })();
    if expanded.is_none() || edges.is_empty() {
        names.insert(UNKNOWN_IMPORTS.to_owned());
    }
    for edge in edges {
        match edge {
            RustPubUseEdge::Named { exported_name, .. } => {
                names.insert(
                    crate::language::normalize_identifier(
                        crate::language::LangId::Rust,
                        &exported_name,
                    )
                    .into_owned(),
                );
            }
            RustPubUseEdge::Wildcard { .. } => {
                // glob の候補名を狭く証明できない場合は package の opaque 戻り値に残す。
                names.insert(UNKNOWN_IMPORTS.to_owned());
            }
        }
    }
    names
}

fn matching_move(moved: &MovedSymbol, path: &str, function: &PublicFunction) -> bool {
    moved.to == path
        && moved.name == function.name
        && moved.kind == function.kind
        && moved.to_inline_module == super::rust_declarations::inline_module_of(&function.context)
}

fn previous_declarations<'a>(
    old: &'a Snapshot,
    key: &Key,
    function: &PublicFunction,
    moved: &[MovedSymbol],
) -> Option<(&'a str, &'a [Declaration])> {
    if let Some((key, declarations)) = old.declarations.get_key_value(key) {
        return Some((key.0.as_str(), declarations.as_slice()));
    }
    let mut candidates = old.declarations.iter().filter(|(old_key, declarations)| {
        moved.iter().any(|movement| {
            matching_move(movement, &key.0, function)
                && movement.from == old_key.0
                && declarations.iter().any(|declaration| {
                    declaration.public.as_ref().is_some_and(|previous| {
                        previous.name == movement.name
                            && previous.kind == movement.kind
                            && super::rust_declarations::inline_module_of(&previous.context)
                                == movement.from_inline_module
                    })
                })
        })
    });
    let (key, declarations) = candidates.next()?;
    candidates
        .next()
        .is_none()
        .then_some((key.0.as_str(), declarations.as_slice()))
}

fn identifiers(node: tree_sitter::Node<'_>, source: &[u8]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let body = matches!(node.kind(), "mod_item" | "trait_item")
        .then(|| node.child_by_field_name("body"))
        .flatten();
    let _: ControlFlow<()> = walk_preorder(node, Children::Named, |node, _| {
        if Some(node) == body {
            return ControlFlow::Continue(Walk::Skip);
        }
        if matches!(
            node.kind(),
            "identifier" | "type_identifier" | "field_identifier"
        ) && let Ok(name) = node.utf8_text(source)
        {
            names.insert(
                crate::language::normalize_identifier(crate::language::LangId::Rust, name)
                    .into_owned(),
            );
        }
        ControlFlow::Continue(Walk::Descend)
    });
    names
}

fn fingerprint(node: tree_sitter::Node<'_>, source: &[u8]) -> blake3::Hash {
    let mut tokens = Vec::new();
    if matches!(node.kind(), "mod_item" | "trait_item") {
        let body = node.child_by_field_name("body");
        let mut cursor = node.walk();
        for child in node
            .children(&mut cursor)
            .filter(|child| Some(*child) != body)
        {
            push_tokens(child, source, &mut tokens);
        }
    } else {
        push_tokens(node, source, &mut tokens);
    }
    let mut previous = node.prev_named_sibling();
    while let Some(attribute) = previous {
        match attribute.kind() {
            "attribute_item" => push_tokens(attribute, source, &mut tokens),
            "line_comment" | "block_comment" => {}
            _ => break,
        }
        previous = attribute.prev_named_sibling();
    }
    let mut hash = blake3::Hasher::new();
    for token in tokens {
        let token = match token.split_once(':') {
            Some((kind @ ("identifier" | "type_identifier" | "field_identifier"), name)) => {
                format!(
                    "{kind}:{}",
                    crate::language::normalize_identifier(crate::language::LangId::Rust, name)
                )
            }
            _ => token,
        };
        hash.update(&(token.len() as u64).to_le_bytes());
        hash.update(token.as_bytes());
    }
    hash.finalize()
}

fn opaque_function(node: tree_sitter::Node<'_>) -> bool {
    node.kind() == "function_item"
        && (node
            .child_by_field_name("return_type")
            .is_some_and(|ty| any_node(ty, Children::Named, |node| node.kind() == "abstract_type"))
            || {
                let mut cursor = node.walk();
                node.named_children(&mut cursor)
                    .find(|child| child.kind() == "function_modifiers")
                    .is_some_and(|modifiers| {
                        any_node(modifiers, Children::All, |node| node.kind() == "async")
                    })
            })
}

impl Snapshot {
    fn add(
        &mut self,
        path: &str,
        source: &[u8],
        strip: &ts_strip_internal::StripInternal,
    ) -> Option<()> {
        let tree = parser::parse_source(source, crate::language::LangId::Rust).ok()?;
        let root = tree.root_node();
        if root.has_error() {
            return None;
        }
        let (symbols, contexts) = extract_deleted_exported_facts(path, source, strip)?;
        let mut exported: BTreeMap<_, _> = symbols
            .into_iter()
            .zip(contexts?)
            .map(|(symbol, context)| (context.start, (symbol, context)))
            .collect();
        // 深さとともにスコープを戻す。各ノードから祖先をたどり直さない。
        let mut scopes: Vec<Scope> = Vec::new();
        let file_conditions = super::rust_declarations::file_availability(root, source);
        let mut condition_scopes: Vec<(usize, Option<Vec<AvailabilityAttr>>)> = Vec::new();
        let _: ControlFlow<()> = walk_preorder(root, Children::Named, |node, depth| {
            while scopes.last().is_some_and(|scope| scope.depth >= depth) {
                scopes.pop();
            }
            while condition_scopes
                .last()
                .is_some_and(|(level, _)| *level >= depth)
            {
                condition_scopes.pop();
            }
            if matches!(node.kind(), "mod_item" | "impl_item" | "trait_item") {
                let field = if node.kind() == "impl_item" {
                    "type"
                } else {
                    "name"
                };
                if let Some(name_node) = node.child_by_field_name(field)
                    && let Ok(name) = name_node.utf8_text(source)
                {
                    let name = if node.kind() == "impl_item" {
                        fingerprint(name_node, source).to_hex().to_string()
                    } else {
                        crate::language::normalize_identifier(crate::language::LangId::Rust, name)
                            .into_owned()
                    };
                    let name = format!("{}:{name}", node.kind());
                    let mut hash = blake3::Hasher::new();
                    if let Some(parent) = scopes.last() {
                        hash.update(parent.key.as_bytes());
                    }
                    hash.update(&(name.len() as u64).to_le_bytes());
                    hash.update(name.as_bytes());
                    scopes.push(Scope {
                        depth,
                        name,
                        key: hash.finalize(),
                    });
                }
            }
            let kind = node.kind();
            let declaration = matches!(
                kind,
                "function_item"
                    | "type_item"
                    | "struct_item"
                    | "enum_item"
                    | "union_item"
                    | "const_item"
                    | "static_item"
                    | "use_declaration"
                    | "macro_definition"
                    | "trait_item"
                    | "mod_item"
            );
            let nested_body = node
                .child_by_field_name("body")
                .filter(|body| body.kind() == "declaration_list");
            if !declaration && nested_body.is_none() {
                return ControlFlow::Continue(Walk::Descend);
            }
            let enclosing = condition_scopes
                .last()
                .map_or(&file_conditions, |(_, conditions)| conditions);
            let own = super::rust_declarations::own_item_availability(node, source);
            let conditions = enclosing
                .as_ref()
                .zip(own)
                .map(|(enclosing, own)| enclosing.iter().cloned().chain(own).collect::<Vec<_>>());
            if let Some(body) = nested_body {
                let mut nested = conditions.clone();
                if kind != "mod_item" {
                    nested = nested
                        .zip(super::rust_declarations::file_availability(body, source))
                        .map(|(mut conditions, inner)| {
                            conditions.extend(inner);
                            conditions
                        });
                }
                condition_scopes.push((depth, nested));
            }
            if !declaration {
                return ControlFlow::Continue(Walk::Descend);
            }
            // 内部キーには祖先の全名を複製せず、親から増分で作った固定長の指紋を使う。
            let scope = scopes
                .last()
                .map_or_else(|| blake3::hash(&[]), |scope| scope.key);
            let imports = format!("@imports:{path}:{}", scope.to_hex());
            let name = if kind == "use_declaration" {
                imports.clone()
            } else {
                let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|name| name.utf8_text(source).ok())
                else {
                    return ControlFlow::Continue(Walk::Skip);
                };
                crate::language::normalize_identifier(crate::language::LangId::Rust, name)
                    .into_owned()
            };
            let conditions = conditions.unwrap_or_default();
            if super::rust_availability::unsatisfiable(&conditions) {
                return ControlFlow::Continue(Walk::Skip);
            }
            let mut references = identifiers(node, source);
            references.insert(imports.clone());
            let bound_names = if kind == "use_declaration" {
                import_bound_names(node, source, path, &scopes)
            } else {
                BTreeSet::new()
            };
            let start = node.start_position();
            let public = if opaque_function(node) {
                exported.remove(&(start.row, start.column)).and_then(
                    |((name, kind, signature), context)| {
                        let body = node.child_by_field_name("body")?;
                        let mut body_references = identifiers(body, source);
                        body_references.insert(imports.clone());
                        body_references.insert(UNKNOWN_IMPORTS.to_owned());
                        Some(PublicFunction {
                            name,
                            kind,
                            signature,
                            context,
                            body: fingerprint(body, source),
                            references: body_references,
                        })
                    },
                )
            } else {
                None
            };
            self.declarations
                .entry((
                    path.to_owned(),
                    *scope.as_bytes(),
                    kind.to_owned(),
                    name.clone(),
                ))
                .or_default()
                .push(Declaration {
                    fingerprint: fingerprint(node, source),
                    name,
                    bound_names,
                    references,
                    conditions,
                    public,
                });
            ControlFlow::Continue(if matches!(kind, "mod_item" | "trait_item") {
                Walk::Descend
            } else {
                Walk::Skip
            })
        });
        Some(())
    }
}

fn remember(changes: &mut ChangedNames, name: &str, conditions: &[AvailabilityAttr]) -> bool {
    let branches = changes.entry(name.to_owned()).or_default();
    if branches.iter().any(|known| known == conditions) {
        return false;
    }
    branches.push(conditions.to_vec());
    true
}

fn remember_declaration(
    changes: &mut ChangedNames,
    pending: &mut VecDeque<(String, Vec<AvailabilityAttr>)>,
    declaration: &Declaration,
) {
    for name in std::iter::once(&declaration.name).chain(&declaration.bound_names) {
        if remember(changes, name, &declaration.conditions) {
            pending.push_back((name.clone(), declaration.conditions.clone()));
        }
    }
}

fn affected(
    references: &BTreeSet<String>,
    conditions: &[AvailabilityAttr],
    changes: &ChangedNames,
) -> bool {
    references.iter().any(|name| {
        changes.get(name).is_some_and(|branches| {
            branches.iter().any(|branch| {
                let combined: Vec<_> = conditions.iter().chain(branch).cloned().collect();
                !super::rust_availability::unsatisfiable(&combined)
            })
        })
    })
}

fn changed_names(old: &Snapshot, new: &Snapshot) -> ChangedNames {
    let mut changes = ChangedNames::new();
    let mut pending = VecDeque::new();
    let mut dependents: BTreeMap<&str, Vec<&Declaration>> = BTreeMap::new();
    for declaration in old
        .declarations
        .values()
        .chain(new.declarations.values())
        .flatten()
    {
        for name in &declaration.references {
            dependents.entry(name).or_default().push(declaration);
        }
    }
    let keys: BTreeSet<_> = old
        .declarations
        .keys()
        .chain(new.declarations.keys())
        .collect();
    for key in keys {
        let fingerprint = |snapshot: &Snapshot| {
            let mut hashes: Vec<_> = snapshot
                .declarations
                .get(key)
                .into_iter()
                .flatten()
                .map(|declaration| *declaration.fingerprint.as_bytes())
                .collect();
            hashes.sort();
            hashes
        };
        let before = fingerprint(old);
        let after = fingerprint(new);
        if before != after {
            for declaration in old
                .declarations
                .get(key)
                .into_iter()
                .flatten()
                .chain(new.declarations.get(key).into_iter().flatten())
            {
                let hash = declaration.fingerprint.as_bytes();
                if before.iter().filter(|known| *known == hash).count()
                    != after.iter().filter(|known| *known == hash).count()
                {
                    remember_declaration(&mut changes, &mut pending, declaration);
                }
            }
        }
    }
    // 変更名から逆向きに伝播し、逆順の長い依存連鎖でも全宣言を繰り返し走査しない。
    while let Some((name, branch)) = pending.pop_front() {
        if let Some(declarations) = dependents.get(name.as_str()) {
            for declaration in declarations {
                let combined: Vec<_> = declaration
                    .conditions
                    .iter()
                    .chain(&branch)
                    .cloned()
                    .collect();
                if !super::rust_availability::unsatisfiable(&combined) {
                    remember_declaration(&mut changes, &mut pending, declaration);
                }
            }
        }
    }
    changes
}

fn incomplete(state: &mut DetectionState<'_>, path: Option<&str>, reason: &str) {
    state.buckets.parse_truncations.push(TruncationInfo::new(path.map(str::to_owned), TruncationReason::RustDependentsIncomplete,
        format!("Rust opaque return bodies and their declaration dependencies were not all compared: {reason}")));
}

/// 隠れた型の変化は、呼び出しが差分内で追随していても外部の auto trait 契約の変更として残す。
pub(super) fn record(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
    moved: &[MovedSymbol],
) {
    let packages: BTreeSet<_> = inputs
        .diff_files
        .iter()
        .zip(inputs.prepared)
        .filter(|(_, prepared)| !matches!(prepared, PreparedDiffFile::Skip))
        .map(|(diff, _)| diff)
        .filter(|df| df.old_path.ends_with(".rs") || df.new_path.ends_with(".rs"))
        .flat_map(|df| [&df.old_path, &df.new_path])
        .filter(|path| path.ends_with(".rs") && crate::engine::impact::is_safe_diff_path(path))
        .filter(|path| {
            !is_binary_only_rust_crate(inputs.dir, path)
                || !is_binary_only_rust_crate_at_base(inputs.base_blobs, path)
        })
        .filter_map(|path| super::rust_dependencies::crates::crate_package(inputs.dir, path))
        .collect();
    if packages.is_empty() {
        return;
    }
    let Some(mut paths) = super::prepare::tracked_files(inputs.dir) else {
        incomplete(state, None, "the tracked source paths could not be listed");
        return;
    };
    paths.extend(
        inputs
            .diff_files
            .iter()
            .flat_map(|df| [df.old_path.clone(), df.new_path.clone()]),
    );
    paths.sort();
    paths.dedup();
    let canonical = std::fs::canonicalize(inputs.dir).ok();
    let resolver = canonical
        .as_deref()
        .map(crate::engine::gitattributes::AttributeResolver::open);
    let mut lookup = resolver.as_ref().map(|resolver| resolver.lookup());
    let mut undetermined = Vec::new();
    let mut snapshots: BTreeMap<String, (Snapshot, Snapshot)> = BTreeMap::new();
    let mut files = 0usize;
    let mut bytes = 0usize;
    for path in paths {
        if !path.ends_with(".rs")
            || is_test_path(std::path::Path::new(&path))
            || !crate::engine::impact::is_safe_diff_path(&path)
        {
            continue;
        }
        let Some(package) = super::rust_dependencies::crates::crate_package(inputs.dir, &path)
            .filter(|package| packages.contains(package))
        else {
            continue;
        };
        let exists = std::path::Path::new(inputs.dir).join(&path).is_file();
        let df = crate::models::impact::DiffFile {
            old_path: path.clone(),
            new_path: if exists {
                path.clone()
            } else {
                "/dev/null".to_owned()
            },
            hunks: Vec::new(),
            deleted_old_source: None,
        };
        if should_skip_diff_file(
            &df,
            lookup.as_mut(),
            canonical.as_deref(),
            &mut undetermined,
        ) {
            continue;
        }
        files += 1;
        if files > MAX_FILES {
            incomplete(state, Some(&package), "more than 10000 source files");
            break;
        }
        let old = inputs.base_blobs.read(&path);
        let new = exists.then(|| load_new_source(inputs.dir, &path)).flatten();
        bytes += old.as_ref().map_or(0, Vec::len) + new.as_ref().map_or(0, |source| source.len());
        if bytes > MAX_BYTES {
            incomplete(state, Some(&package), "more than 64 MiB of source text");
            break;
        }
        let pair = snapshots.entry(package).or_default();
        for (snapshot, source) in [(&mut pair.0, old.as_deref()), (&mut pair.1, new.as_deref())] {
            let Some(source) = source else {
                continue;
            };
            if source.len() > MAX_FILE_BYTES
                || snapshot.add(&path, source, inputs.strip_internal).is_none()
            {
                incomplete(
                    state,
                    Some(&path),
                    "a source exceeds 2 MiB or could not be parsed",
                );
            }
        }
        if exists && new.is_none() {
            incomplete(state, Some(&path), "a current source could not be read");
        }
    }
    if let Some(root) = canonical.as_deref() {
        state.buckets.parse_truncations.extend(
            crate::engine::refs::generated_undetermined_truncations(root, &undetermined),
        );
    }
    for (old, new) in snapshots.values() {
        let changes = changed_names(old, new);
        for (key, current) in &new.declarations {
            let after = public_functions(current);
            let Some(&new_function) = after.first() else {
                continue;
            };
            let previous = previous_declarations(old, key, new_function, moved);
            if previous.is_none()
                && !moved
                    .iter()
                    .any(|movement| matching_move(movement, &key.0, new_function))
            {
                continue;
            }
            let before =
                previous.map_or_else(Vec::new, |(_, declarations)| public_functions(declarations));
            let old_function = before.first().copied();
            let body_changed = before.is_empty() || bodies_changed(&before, &after);
            let dependency_changed = current
                .iter()
                .chain(
                    previous
                        .into_iter()
                        .flat_map(|(_, declarations)| declarations),
                )
                .any(|declaration| {
                    declaration.public.as_ref().is_some_and(|function| {
                        affected(&function.references, &declaration.conditions, &changes)
                    })
                });
            if !body_changed && !dependency_changed {
                continue;
            }
            let (path, _, _, _) = key;
            if rust_declaration_outside_public_api_surface(
                inputs.rust_crates,
                state.rust_public,
                inputs.dir,
                inputs.base_blobs,
                false,
                path,
                &new_function.name,
                Some(&new_function.context),
            ) && old_function.is_some_and(|old_function| {
                rust_declaration_outside_public_api_surface(
                    inputs.rust_crates,
                    state.rust_public,
                    inputs.dir,
                    inputs.base_blobs,
                    true,
                    previous.map_or(path.as_str(), |(path, _)| path),
                    &old_function.name,
                    Some(&old_function.context),
                )
            }) {
                continue;
            }
            let inline_module = super::rust_declarations::inline_module_of(&new_function.context);
            let same = |change: &ApiSymbolChange| {
                change.file == *path
                    && change.name == new_function.name
                    && change.inline_module == inline_module
            };
            if state.buckets.modified.iter().any(same) {
                continue;
            }
            state
                .buckets
                .modified_closed_in_diff
                .retain(|change| !same(change));
            state.buckets.compatible_modified.retain(|change| {
                !(change.file == *path
                    && change.name == new_function.name
                    && change.inline_module == inline_module)
            });
            state.buckets.modified.push(ApiSymbolChange {
                inline_module,
                unverified: Some(ApiModificationUnverified {
                    reason: ApiModificationUnverifiedReason::RustSignatureDependencyUnresolved,
                    first_unproven: None,
                }),
                ..ApiSymbolChange::new(
                    new_function.name.clone(),
                    new_function.kind.clone(),
                    path.clone(),
                    old_function.map(|function| function.signature.clone()),
                    Some(new_function.signature.clone()),
                )
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::tests::common::*;

    const CARGO: &str = "[package]\nname = \"opaque_demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[features]\nwide = []\n";

    fn changes(old: &[(&str, &str)], new: &[(&str, &str)]) -> ApiChanges {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_git_repo_for_test(repo);
        let files: Vec<_> = std::iter::once(("Cargo.toml", CARGO))
            .chain(old.iter().copied())
            .collect();
        git_commit_files(repo, &files, "base");
        for (path, source) in new {
            std::fs::write(repo.join(path), source).unwrap();
        }
        detect_api_changes_from_worktree(repo)
    }

    fn modified(api: &ApiChanges, name: &str) -> bool {
        api.modified.iter().any(|change| change.name == name)
    }

    #[test]
    fn opaque_returns_report_direct_body_and_private_alias_changes() {
        let before = "type Hidden = ();\npub fn indirect() -> impl Copy { Hidden::default() }\npub fn direct() -> impl Copy { 0u8 }\npub fn ordinary() -> u8 { 0 }\n";
        let after = "type Hidden = core::marker::PhantomData<std::rc::Rc<()>>;\npub fn indirect() -> impl Copy { Hidden::default() }\npub fn direct() -> impl Copy { core::marker::PhantomData::<std::rc::Rc<()>> }\npub fn ordinary() -> u8 { 1 }\n";
        let api = changes(&[("src/lib.rs", before)], &[("src/lib.rs", after)]);
        assert!(modified(&api, "indirect"), "{api:?}");
        assert!(modified(&api, "direct"), "{api:?}");
        assert!(!modified(&api, "ordinary"), "{api:?}");
        assert!(
            api.modified_closed_in_diff
                .iter()
                .all(|change| !matches!(change.name.as_str(), "indirect" | "direct")),
            "{api:?}"
        );
    }

    #[test]
    fn opaque_returns_follow_other_file_helpers_and_async_bodies() {
        let root = "mod hidden;\npub fn produce() -> impl Copy { hidden::make() }\npub async fn future() { hidden::wait().await; }\npub fn ordinary() -> u8 { hidden::plain() }\n";
        let before = "type Hidden = ();\npub fn make() -> Hidden { Hidden::default() }\npub async fn wait() { let held = (); std::future::ready(()).await; drop(held); }\npub fn plain() -> u8 { 0 }\n";
        let after = "type Hidden = core::marker::PhantomData<std::rc::Rc<()>>;\npub fn make() -> Hidden { Hidden::default() }\npub async fn wait() { let held = std::rc::Rc::new(()); std::future::ready(()).await; drop(held); }\npub fn plain() -> u8 { 1 }\n";
        let api = changes(
            &[("src/lib.rs", root), ("src/hidden.rs", before)],
            &[("src/hidden.rs", after)],
        );
        assert!(modified(&api, "produce"), "{api:?}");
        assert!(modified(&api, "future"), "{api:?}");
        assert!(!modified(&api, "ordinary"), "{api:?}");
        assert!(!modified(&api, "make"), "{api:?}");
        for (imported, name) in [
            ("Hidden", "crate::Hidden"),
            ("Hidden as Selected", "crate::Selected"),
            ("*", "Hidden"),
        ] {
            let root = format!(
                "mod a {{ pub type Hidden = (); }}\nmod b {{ pub type Hidden = core::marker::PhantomData<std::rc::Rc<()>>; }}\nuse a::{imported};\npub mod api;\n"
            );
            let api_source = format!(
                "{}pub fn produce() -> impl Copy {{ {name}::default() }}\n",
                if imported == "*" {
                    "use super::*;\n"
                } else {
                    ""
                },
            );
            let changed = root.replace("use a::", "use b::");
            let api = changes(
                &[("src/lib.rs", &root), ("src/api.rs", &api_source)],
                &[("src/lib.rs", &changed)],
            );
            assert!(modified(&api, "produce"), "{imported}: {api:?}");
        }
        let root = "mod a { pub type Other = (); }\nmod b { pub type Other = u8; }\nuse a::Other;\npub mod api;\n";
        let changed = root.replace("use a::", "use b::");
        let api = changes(
            &[
                ("src/lib.rs", root),
                (
                    "src/api.rs",
                    "type Hidden = ();\npub fn produce() -> impl Copy { Hidden::default() }\n",
                ),
            ],
            &[("src/lib.rs", &changed)],
        );
        assert!(!modified(&api, "produce"), "{api:?}");
    }

    #[test]
    fn opaque_returns_propagate_reverse_order_dependency_chains() {
        let mut source = String::new();
        for index in 0..512 {
            source.push_str(&format!("type A{index:04} = A{:04};\n", index + 1));
        }
        source.push_str("type A0512 = ();\ntype Unrelated = ();\npub fn produce() -> impl Copy { A0000::default() }\n");
        let after = source.replace("type A0512 = ();", "type A0512 = u8;");
        let strip = ts_strip_internal::StripInternal::disabled();
        let mut old = Snapshot::default();
        let mut new = Snapshot::default();
        old.add("src/lib.rs", source.as_bytes(), &strip).unwrap();
        new.add("src/lib.rs", after.as_bytes(), &strip).unwrap();
        let changed = changed_names(&old, &new);
        assert!(changed.contains_key("A0000"));
        assert!(changed.contains_key("produce"));
        assert!(!changed.contains_key("Unrelated"));
        assert!(changed_names(&old, &old).is_empty());
    }

    #[test]
    fn opaque_returns_keep_deep_private_module_dependencies_local() {
        let modules: Vec<_> = (0..512).map(|index| format!("m{index:04}")).collect();
        let mut source = String::new();
        for name in &modules {
            source.push_str(&format!("mod {name} {{\n"));
        }
        source.push_str("type Hidden = ();\npub fn produce() -> impl Copy { Hidden::default() }\n");
        source.push_str(&"}\n".repeat(modules.len()));
        let after = source.replace("type Hidden = ();", "type Hidden = u8;");
        let strip = ts_strip_internal::StripInternal::disabled();
        let mut old = Snapshot::default();
        let mut new = Snapshot::default();
        old.add("src/lib.rs", source.as_bytes(), &strip).unwrap();
        new.add("src/lib.rs", after.as_bytes(), &strip).unwrap();
        let changed = changed_names(&old, &new);
        assert!(changed.contains_key("produce"));
        assert!(!changed.contains_key("m0000"));
        let function = new
            .declarations
            .values()
            .flatten()
            .find_map(|declaration| declaration.public.as_ref())
            .unwrap();
        assert!(affected(&function.references, &[], &changed));
        assert_eq!(
            super::super::rust_declarations::inline_module_of(&function.context).as_deref(),
            Some(modules.join("::").as_str()),
        );
        assert!(changed_names(&old, &old).is_empty());
    }

    #[test]
    fn opaque_returns_keep_format_comments_unrelated_and_inactive_changes_outside() {
        let before = "type Hidden = ();\nfn helper() -> Hidden { Hidden::default() }\nfn unrelated() -> u8 { 0 }\npub fn produce() -> impl Copy { helper() }\npub fn ordinary() -> u8 { 0 }\n";
        let after = "type Hidden = /* same */ ( );\nfn helper() -> Hidden { /* same */ Hidden :: default ( ) }\nfn unrelated() -> u8 { 1 }\npub fn produce() -> impl Copy {\n helper() // same\n}\npub fn ordinary() -> u8 { 1 }\n";
        let api = changes(&[("src/lib.rs", before)], &[("src/lib.rs", after)]);
        assert!(api.modified.is_empty(), "{api:?}");
        let before = "#[cfg(feature = \"wide\")]\ntype Hidden = ();\n#[cfg(not(feature = \"wide\"))]\ntype Hidden = u8;\n#[cfg(feature = \"wide\")]\npub fn produce() -> impl Copy { Hidden::default() }\n";
        let after = before.replace("type Hidden = u8;", "type Hidden = u16;");
        let api = changes(&[("src/lib.rs", before)], &[("src/lib.rs", &after)]);
        assert!(!modified(&api, "produce"), "{api:?}");
        for header in [
            "#[cfg(feature = \"wide\")]\npub mod nested {\n",
            "pub mod nested {\n#![cfg(feature = \"wide\")]\n",
        ] {
            let before = format!(
                "{header}#[cfg(feature = \"wide\")]\ntype Hidden = ();\n#[cfg(not(feature = \"wide\"))]\ntype Hidden = u8;\npub fn produce() -> impl Copy {{ Hidden::default() }}\n}}\n"
            );
            let after = before.replace("type Hidden = u8;", "type Hidden = u16;");
            let api = changes(&[("src/lib.rs", &before)], &[("src/lib.rs", &after)]);
            assert!(!modified(&api, "produce"), "{header}: {api:?}");
            let after = before.replace("type Hidden = ();", "type Hidden = u32;");
            let api = changes(&[("src/lib.rs", &before)], &[("src/lib.rs", &after)]);
            assert!(modified(&api, "produce"), "{header}: {api:?}");
        }
        let before = "#[cfg(feature = \"wide\")]\npub fn produce() -> impl Copy { 0u8 }\n";
        let after = before.replace("cfg(feature = \"wide\")", "cfg(all(feature = \"wide\"))");
        let api = changes(&[("src/lib.rs", before)], &[("src/lib.rs", &after)]);
        assert!(!modified(&api, "produce"), "{api:?}");
    }

    #[test]
    fn opaque_return_moves_compare_the_verified_source_body() {
        let root = "pub mod a;\npub mod b;\npub use a::produce;\n";
        let destination_root = root.replace("a::produce", "b::produce");
        let before = "pub fn produce() -> impl Copy { 0u8 }\n";
        for (body, changed) in [
            ("0u8", false),
            ("core::marker::PhantomData::<std::rc::Rc<()>>", true),
        ] {
            let after =
                format!("pub fn anchor() {{}}\npub fn produce() -> impl Copy {{ {body} }}\n");
            let api = changes(
                &[
                    ("src/lib.rs", root),
                    ("src/a.rs", before),
                    ("src/b.rs", "pub fn anchor() {}\n"),
                ],
                &[
                    ("src/lib.rs", &destination_root),
                    ("src/a.rs", "// moved\n"),
                    ("src/b.rs", &after),
                ],
            );
            assert!(
                api.moved.iter().any(|movement| movement.name == "produce"),
                "{api:?}"
            );
            assert_eq!(modified(&api, "produce"), changed, "{body}: {api:?}");
        }
    }

    #[test]
    fn opaque_returns_keep_cfg_conditions_paired_with_each_body() {
        let before = "#[cfg(feature = \"wide\")]\npub fn produce() -> impl Copy { 0u8 }\n#[cfg(not(feature = \"wide\"))]\npub fn produce() -> impl Copy { core::marker::PhantomData::<std::rc::Rc<()>> }\n";
        let after = "#[cfg(feature = \"wide\")]\npub fn produce() -> impl Copy { core::marker::PhantomData::<std::rc::Rc<()>> }\n#[cfg(not(feature = \"wide\"))]\npub fn produce() -> impl Copy { 0u8 }\n";
        let api = changes(&[("src/lib.rs", before)], &[("src/lib.rs", after)]);
        assert!(modified(&api, "produce"), "{api:?}");
    }

    #[test]
    fn opaque_returns_follow_trait_headers_and_module_path_attributes() {
        let before = "trait Marker: Send {}\ntype Hidden = core::marker::PhantomData<Box<dyn Marker>>;\npub fn produce() -> impl Copy { Hidden::default() }\n";
        let after = before.replace("Marker: Send", "Marker");
        let api = changes(&[("src/lib.rs", before)], &[("src/lib.rs", &after)]);
        assert!(modified(&api, "produce"), "{api:?}");
        // 中身を変えない trait のコメント・整形は、指紋を変えない。
        let after = before.replace("Marker: Send", "Marker : /* same */ Send");
        let api = changes(&[("src/lib.rs", before)], &[("src/lib.rs", &after)]);
        assert!(!modified(&api, "produce"), "{api:?}");
        let root = "#[path = \"first.rs\"]\nmod selected;\npub fn produce() -> impl Copy { selected::make() }\n";
        let after = root.replace("first.rs", "second.rs");
        let api = changes(
            &[
                ("src/lib.rs", root),
                ("src/first.rs", "pub fn make() -> impl Copy { () }\n"),
                (
                    "src/second.rs",
                    "pub fn make() -> impl Copy { core::marker::PhantomData::<std::rc::Rc<()>> }\n",
                ),
            ],
            &[("src/lib.rs", &after)],
        );
        assert!(modified(&api, "produce"), "{api:?}");
    }
}
