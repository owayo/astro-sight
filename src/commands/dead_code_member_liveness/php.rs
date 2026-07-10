use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::engine::parser;
use crate::language::LangId;

use super::{
    DuplicateSetResult, MemberCandidate, MemberStatus, collect_source_files, is_class_member_kind,
    status_from_counts,
};

/// PHP class member の owner-aware liveness インデックス。
///
/// PHP はメソッド名が case-insensitive で、`new` のような言語キーワードと同じ文字列を
/// メソッド名として持てる。duplicate bare name では通常の refs count だけでは owner を
/// 区別できないため、確定的に解決できる `Owner::method()` / 同一クラス内の
/// `self::method()` / `static::method()` だけを数える。
#[derive(Debug, Default)]
pub(crate) struct PhpMemberLiveness {
    statuses: HashMap<(String, String, String), MemberStatus>,
}

impl PhpMemberLiveness {
    pub(crate) fn build<F>(
        candidates: &[(String, String, String, LangId)],
        canonical_dir: &Path,
        extra_files: &[std::path::PathBuf],
        is_test_path: F,
    ) -> Self
    where
        F: Fn(&Path) -> bool,
    {
        let mut statuses: HashMap<(String, String, String), MemberStatus> = HashMap::new();
        let php_members = collect_php_member_candidates(candidates);
        if php_members.is_empty() {
            return Self { statuses };
        }

        let bare_to_members = group_php_members_by_bare(&php_members);
        if !has_duplicate_php_member_set(&bare_to_members) {
            return Self { statuses };
        }

        let Some(php_files) = collect_php_files(canonical_dir, extra_files) else {
            return Self { statuses };
        };
        let trait_uses = collect_php_trait_uses(&php_files);

        for (bare_key, members) in &bare_to_members {
            let owners: HashSet<String> = members.iter().map(|m| php_fold_name(&m.owner)).collect();
            if owners.len() < 2 {
                continue;
            }
            let analysis = analyze_php_duplicate_set(
                bare_key,
                &owners,
                &php_files,
                &trait_uses,
                &is_test_path,
            );

            match analysis {
                DuplicateSetResult::Ambiguous => {
                    for m in members {
                        statuses.insert(
                            (
                                m.file.clone(),
                                php_fold_name(&m.owner),
                                php_fold_name(&m.bare),
                            ),
                            MemberStatus::Ambiguous,
                        );
                    }
                }
                DuplicateSetResult::Counted(counts) => {
                    for m in members {
                        let owner_key = php_fold_name(&m.owner);
                        let (prod, tst) = counts.get(&owner_key).copied().unwrap_or((0, 0));
                        let status = status_from_counts(prod, tst);
                        statuses
                            .insert((m.file.clone(), owner_key, php_fold_name(&m.bare)), status);
                    }
                }
            }
        }

        Self { statuses }
    }

    pub(crate) fn status_for(&self, owner: &str, bare: &str, file: &str) -> Option<MemberStatus> {
        self.statuses
            .get(&(file.to_string(), php_fold_name(owner), php_fold_name(bare)))
            .copied()
    }
}

fn collect_php_member_candidates(
    candidates: &[(String, String, String, LangId)],
) -> Vec<MemberCandidate> {
    let mut php_members = Vec::new();
    for (name, kind, file, lang) in candidates {
        if *lang != LangId::Php || !is_class_member_kind(kind) {
            continue;
        }
        let Some((owner, bare)) = name.rsplit_once('.') else {
            continue;
        };
        if owner.contains('.') {
            continue;
        }
        php_members.push(MemberCandidate {
            owner: owner.to_string(),
            bare: bare.to_string(),
            file: file.clone(),
        });
    }
    php_members
}

fn group_php_members_by_bare(
    members: &[MemberCandidate],
) -> HashMap<String, Vec<&MemberCandidate>> {
    let mut grouped: HashMap<String, Vec<&MemberCandidate>> = HashMap::new();
    for m in members {
        grouped.entry(php_fold_name(&m.bare)).or_default().push(m);
    }
    grouped
}

fn has_duplicate_php_member_set(grouped: &HashMap<String, Vec<&MemberCandidate>>) -> bool {
    grouped.values().any(|v| {
        let owners: HashSet<String> = v.iter().map(|m| php_fold_name(&m.owner)).collect();
        owners.len() >= 2
    })
}

fn collect_php_files(
    canonical_dir: &Path,
    extra_files: &[std::path::PathBuf],
) -> Option<Vec<std::path::PathBuf>> {
    let files = collect_source_files(canonical_dir, extra_files)?;
    Some(
        files
            .into_iter()
            .filter(|p| {
                let Some(s) = p.to_str() else {
                    return false;
                };
                matches!(LangId::from_path(camino::Utf8Path::new(s)), Ok(LangId::Php))
            })
            .collect(),
    )
}

#[derive(Default)]
struct PhpTraitUses {
    traits: HashSet<String>,
    has_adaptation: bool,
    ambiguous: bool,
    /// 本体直下に宣言された**具象**メソッド名 (folded)。PHP の解決順は
    /// 「自クラス > trait > 親」のため、具象の同名宣言があると trait 側へは到達しない
    /// (abstract 宣言は trait 実装で満たされるため含めない)。
    declared_methods: HashSet<String>,
}

/// PHP の class/trait 本体直下にある trait `use` を owner 名ごとに収集する。
/// 同名 owner の複数宣言や parse 不能な use は dispatch 先を一意に決められないため、
/// 後段で `Ambiguous` に倒す情報として保持する。
fn collect_php_trait_uses(files: &[std::path::PathBuf]) -> HashMap<String, PhpTraitUses> {
    let mut uses_by_owner = HashMap::new();
    for file_path in files {
        let Some(path) = file_path.to_str() else {
            continue;
        };
        let Ok(source) = parser::read_file(camino::Utf8Path::new(path)) else {
            continue;
        };
        let Ok(tree) = parser::parse_source(&source, LangId::Php) else {
            continue;
        };
        collect_php_trait_uses_from_node(tree.root_node(), source.as_bytes(), &mut uses_by_owner);
    }
    uses_by_owner
}

fn collect_php_trait_uses_from_node(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    uses_by_owner: &mut HashMap<String, PhpTraitUses>,
) {
    // enum (PHP 8.1+) も trait を use できるため収集対象に含める
    // (name/body フィールドは class と同形)。
    if matches!(
        node.kind(),
        "class_declaration" | "trait_declaration" | "enum_declaration"
    ) && let Some(owner) = node
        .child_by_field_name("name")
        .and_then(|name| php_node_key(name, source))
        && let Some(body) = node.child_by_field_name("body")
    {
        let mut collected = PhpTraitUses::default();
        let mut body_cursor = body.walk();
        for declaration in body.named_children(&mut body_cursor) {
            if declaration.kind() == "method_declaration" {
                // 具象メソッドのみ収集 (abstract は trait 実装で満たされ shadow しない)。
                let is_abstract = {
                    let mut method_cursor = declaration.walk();
                    declaration
                        .named_children(&mut method_cursor)
                        .any(|c| c.kind() == "abstract_modifier")
                };
                if !is_abstract
                    && let Some(method_name) = declaration
                        .child_by_field_name("name")
                        .and_then(|name| php_node_key(name, source))
                {
                    collected.declared_methods.insert(method_name);
                }
                continue;
            }
            if declaration.kind() != "use_declaration" {
                continue;
            }
            let mut use_cursor = declaration.walk();
            for child in declaration.named_children(&mut use_cursor) {
                match child.kind() {
                    "name" | "qualified_name" => {
                        if let Some(trait_name) = php_node_key(child, source) {
                            collected.traits.insert(trait_name);
                        } else {
                            collected.ambiguous = true;
                        }
                    }
                    "use_list" => collected.has_adaptation = true,
                    _ => collected.ambiguous = true,
                }
            }
        }
        if !collected.traits.is_empty() || collected.has_adaptation || collected.ambiguous {
            use std::collections::hash_map::Entry;
            match uses_by_owner.entry(owner) {
                Entry::Vacant(entry) => {
                    entry.insert(collected);
                }
                Entry::Occupied(mut entry) => {
                    let existing = entry.get_mut();
                    existing.traits.extend(collected.traits);
                    existing.has_adaptation |= collected.has_adaptation;
                    existing.ambiguous = true;
                }
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_php_trait_uses_from_node(child, source, uses_by_owner);
    }
}

fn analyze_php_duplicate_set<F>(
    bare_key: &str,
    owners: &HashSet<String>,
    files: &[std::path::PathBuf],
    trait_uses: &HashMap<String, PhpTraitUses>,
    is_test_path: F,
) -> DuplicateSetResult
where
    F: Fn(&Path) -> bool,
{
    let mut counts: HashMap<String, (usize, usize)> = HashMap::new();

    for file_path in files {
        let utf8 = match file_path.to_str() {
            Some(s) => camino::Utf8Path::new(s),
            None => continue,
        };
        let source = match parser::read_file(utf8) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if !contains_ascii_case_insensitive(source.as_bytes(), bare_key.as_bytes()) {
            continue;
        }
        let tree = match parser::parse_source(&source, LangId::Php) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let mut analysis = PhpFileAnalysis::default();
        visit_php_node(
            tree.root_node(),
            source.as_bytes(),
            owners,
            bare_key,
            None,
            trait_uses,
            &mut analysis,
        );
        if analysis.ambiguous {
            return DuplicateSetResult::Ambiguous;
        }
        if analysis.scoped_counts.is_empty() {
            continue;
        }
        let is_test = is_test_path(file_path.as_path());
        for (owner, count) in analysis.scoped_counts {
            let entry = counts.entry(owner).or_insert((0, 0));
            if is_test {
                entry.1 = entry.1.saturating_add(count);
            } else {
                entry.0 = entry.0.saturating_add(count);
            }
        }
    }

    DuplicateSetResult::Counted(counts)
}

#[derive(Default)]
struct PhpFileAnalysis {
    scoped_counts: HashMap<String, usize>,
    ambiguous: bool,
}

fn visit_php_node(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    owners: &HashSet<String>,
    bare_key: &str,
    current_class: Option<&str>,
    trait_uses: &HashMap<String, PhpTraitUses>,
    analysis: &mut PhpFileAnalysis,
) {
    if analysis.ambiguous {
        return;
    }

    let current_class_buf = php_class_context_for_node(node, source, owners, bare_key, trait_uses);
    let next_class = current_class_buf.as_deref().or(current_class);

    process_php_liveness_node(
        node, source, owners, bare_key, next_class, trait_uses, analysis,
    );

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit_php_node(
            child, source, owners, bare_key, next_class, trait_uses, analysis,
        );
        if analysis.ambiguous {
            break;
        }
    }
}

fn php_class_context_for_node(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    owners: &HashSet<String>,
    bare_key: &str,
    trait_uses: &HashMap<String, PhpTraitUses>,
) -> Option<String> {
    if !is_php_type_declaration(node.kind()) {
        return None;
    }
    node.child_by_field_name("name")
        .and_then(|name| php_node_key(name, source))
        .filter(|key| {
            !matches!(
                php_resolve_trait_dispatch(key, bare_key, owners, trait_uses),
                PhpOwnerResolution::Ignore
            )
        })
}

fn process_php_liveness_node(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    owners: &HashSet<String>,
    bare_key: &str,
    current_class: Option<&str>,
    trait_uses: &HashMap<String, PhpTraitUses>,
    analysis: &mut PhpFileAnalysis,
) {
    match node.kind() {
        "scoped_call_expression" => {
            if php_call_name_matches(node, source, bare_key) {
                record_php_scoped_call(
                    node,
                    source,
                    owners,
                    bare_key,
                    current_class,
                    trait_uses,
                    analysis,
                );
            }
        }
        "member_call_expression" => {
            if php_call_name_matches(node, source, bare_key) {
                analysis.ambiguous = true;
            }
        }
        "string_content" => {
            if let Ok(text) = node.utf8_text(source)
                && php_string_content_mentions_method(text, bare_key)
            {
                analysis.ambiguous = true;
            }
        }
        _ => {}
    }
}

fn record_php_scoped_call(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    owners: &HashSet<String>,
    bare_key: &str,
    current_class: Option<&str>,
    trait_uses: &HashMap<String, PhpTraitUses>,
    analysis: &mut PhpFileAnalysis,
) {
    match php_scoped_call_owner(node, source, owners, bare_key, current_class, trait_uses) {
        PhpOwnerResolution::Resolved(owner) => {
            *analysis.scoped_counts.entry(owner).or_default() += 1;
        }
        PhpOwnerResolution::Ambiguous => analysis.ambiguous = true,
        PhpOwnerResolution::Ignore => {}
    }
}

enum PhpOwnerResolution {
    Resolved(String),
    Ignore,
    Ambiguous,
}

fn php_scoped_call_owner(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    owners: &HashSet<String>,
    bare_key: &str,
    current_class: Option<&str>,
    trait_uses: &HashMap<String, PhpTraitUses>,
) -> PhpOwnerResolution {
    let Some(scope) = node
        .child_by_field_name("scope")
        .or_else(|| node.named_child(0))
    else {
        return PhpOwnerResolution::Ambiguous;
    };
    let Ok(text) = scope.utf8_text(source) else {
        return PhpOwnerResolution::Ambiguous;
    };
    let folded = php_fold_name(
        text.trim_start_matches('\\')
            .rsplit('\\')
            .next()
            .unwrap_or(text),
    );
    match folded.as_str() {
        "self" => current_class
            .map(|owner| php_resolve_trait_dispatch(owner, bare_key, owners, trait_uses))
            .unwrap_or(PhpOwnerResolution::Ambiguous),
        // `static::` は遅延静的束縛 (late static binding) でサブクラス override へ
        // ディスパッチされ得る。継承グラフを持たない本解析では enclosing class へ
        // 確定解決するとサブクラス側メソッドの dead 誤検出になるため `parent::` と
        // 同じく Ambiguous に倒す (duplicate set 全体が旧スキップへフォールバック)。
        "parent" | "static" => PhpOwnerResolution::Ambiguous,
        _ if owners.contains(&folded) => PhpOwnerResolution::Resolved(folded),
        _ if text.starts_with('$') => PhpOwnerResolution::Ambiguous,
        _ => php_resolve_trait_dispatch(&folded, bare_key, owners, trait_uses),
    }
}

fn php_resolve_trait_dispatch(
    dispatch_owner: &str,
    bare_key: &str,
    owners: &HashSet<String>,
    trait_uses: &HashMap<String, PhpTraitUses>,
) -> PhpOwnerResolution {
    let mut visiting = HashSet::new();
    let mut matching = HashSet::new();
    let mut ambiguous = false;
    collect_php_trait_dispatch_targets(
        dispatch_owner,
        bare_key,
        owners,
        trait_uses,
        &mut visiting,
        &mut matching,
        &mut ambiguous,
    );
    if matching.is_empty() {
        return PhpOwnerResolution::Ignore;
    }
    if ambiguous || matching.len() != 1 {
        return PhpOwnerResolution::Ambiguous;
    }
    PhpOwnerResolution::Resolved(matching.into_iter().next().unwrap())
}

fn collect_php_trait_dispatch_targets(
    dispatch_owner: &str,
    bare_key: &str,
    owners: &HashSet<String>,
    trait_uses: &HashMap<String, PhpTraitUses>,
    visiting: &mut HashSet<String>,
    matching: &mut HashSet<String>,
    ambiguous: &mut bool,
) {
    if owners.contains(dispatch_owner) {
        matching.insert(dispatch_owner.to_string());
        return;
    }
    let Some(composition) = trait_uses.get(dispatch_owner) else {
        return;
    };
    // 合成先が同名の**具象**メソッドを自己宣言している場合、PHP の解決順
    // (自クラス > trait) により trait 側へは到達しない。candidate へ辿らず打ち切る
    // (自己宣言メソッドは candidate ではないため票は入らない = Ignore 方向)。
    if composition.declared_methods.contains(bare_key) {
        return;
    }
    if !visiting.insert(dispatch_owner.to_string()) {
        *ambiguous = true;
        return;
    }
    if composition.has_adaptation || composition.ambiguous {
        *ambiguous = true;
    }
    for trait_name in &composition.traits {
        collect_php_trait_dispatch_targets(
            trait_name, bare_key, owners, trait_uses, visiting, matching, ambiguous,
        );
    }
    visiting.remove(dispatch_owner);
}

fn php_call_name_matches(node: tree_sitter::Node<'_>, source: &[u8], bare_key: &str) -> bool {
    node.child_by_field_name("name")
        .and_then(|name| php_node_key(name, source))
        .is_some_and(|key| key == bare_key)
}

fn php_node_key(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let text = node.utf8_text(source).ok()?;
    Some(php_fold_name(
        text.trim_start_matches('\\')
            .rsplit('\\')
            .next()
            .unwrap_or(text),
    ))
}

fn php_string_content_mentions_method(text: &str, bare_key: &str) -> bool {
    let folded = text.trim().to_ascii_lowercase();
    folded == bare_key
        || folded.ends_with(&format!("::{bare_key}"))
        || folded.ends_with(&format!("@{bare_key}"))
}

fn contains_ascii_case_insensitive(haystack: &[u8], needle_lower: &[u8]) -> bool {
    !needle_lower.is_empty()
        && haystack
            .windows(needle_lower.len())
            .any(|window| window.eq_ignore_ascii_case(needle_lower))
}

fn php_fold_name(name: &str) -> String {
    name.to_ascii_lowercase()
}

fn is_php_type_declaration(kind: &str) -> bool {
    matches!(
        kind,
        "class_declaration" | "interface_declaration" | "trait_declaration" | "enum_declaration"
    )
}
