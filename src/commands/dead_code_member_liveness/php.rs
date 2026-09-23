use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::engine::{parser, refs};
use crate::language::LangId;

use super::{
    MemberCandidate, MemberStatus, SetAccum, collect_source_files, is_class_member_kind,
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
        F: Fn(&Path) -> bool + Sync,
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
        let types = collect_php_type_index(&php_files);

        // duplicate set を先に確定し、ファイル毎に 1 回だけ read + parse して全 set を
        // まとめて解析する (ループ反転、js_ts 側と同じ構成)。旧実装は set 毎に全 PHP
        // ファイルを read + parse しており、set 数 × ファイル数の再パースになっていた。
        let sets: Vec<(&str, HashSet<String>)> = bare_to_members
            .iter()
            .filter_map(|(bare_key, members)| {
                let owners: HashSet<String> =
                    members.iter().map(|m| php_fold_name(&m.owner)).collect();
                if owners.len() < 2 {
                    return None;
                }
                Some((bare_key.as_str(), owners))
            })
            .collect();

        let accums = analyze_php_sets_over_files(&sets, &php_files, &types, &is_test_path);

        for ((bare_key, _owners), accum) in sets.iter().zip(accums) {
            let members = &bare_to_members[*bare_key];
            if accum.ambiguous {
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
            } else {
                for m in members {
                    let owner_key = php_fold_name(&m.owner);
                    let (prod, tst) = accum.counts.get(&owner_key).copied().unwrap_or((0, 0));
                    let status = status_from_counts(prod, tst);
                    statuses.insert((m.file.clone(), owner_key, php_fold_name(&m.bare)), status);
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
    // 拡張子なしの `bin/console` (`#!/usr/bin/env php`) も参照件数の経路と同じく含める。
    Some(
        files
            .into_iter()
            .filter(|p| refs::detect_source_lang(p) == Some(LangId::Php))
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

/// PHP の型宣言 (class / trait / enum / interface) 1 名分の、scope 解決に使う事実。
///
/// 候補 owner でも trait 合成先でもないクラスを scope にした参照 (`Child::make()` /
/// `new Child()`) が、継承元の候補メソッドへ到達し得るかを判定する材料
/// (`php_resolve_unowned_scope`)。
#[derive(Default)]
struct PhpTypeDecl {
    /// 同名 (folded) の宣言数。namespace 違いの同名クラスは区別できないため、
    /// 2 以上なら宣言ごとの事実を当てにしない。
    count: usize,
    /// `extends` (base_clause) を持つ宣言があるか。親の実装を継承し得る。
    has_parent: bool,
    /// 本体直下に**実装付き**で宣言されたメソッド名 (folded)。abstract 宣言と
    /// interface のメソッド宣言は本体を持たないので含めない。
    concrete_methods: HashSet<String>,
}

impl PhpTypeDecl {
    /// 同名宣言の事実を合算する。加算と和集合だけなのでマージ順に依らず決定的。
    fn merge(&mut self, other: PhpTypeDecl) {
        self.count += other.count;
        self.has_parent |= other.has_parent;
        self.concrete_methods.extend(other.concrete_methods);
    }
}

/// member liveness の scope 解決に使う、走査対象の PHP 型宣言の索引。
#[derive(Default)]
struct PhpTypeIndex {
    /// owner (class / trait / enum) 名 → trait `use` の合成情報。
    trait_uses: HashMap<String, PhpTraitUses>,
    /// 型名 → 宣言の事実 (interface を含む全型宣言)。
    decls: HashMap<String, PhpTypeDecl>,
}

/// PHP ファイル群から型宣言の索引を作る。trait `use` は owner 名ごとに収集し、
/// 同名 owner の複数宣言や parse 不能な use は dispatch 先を一意に決められないため、
/// 後段で `Ambiguous` に倒す情報として保持する。
fn collect_php_type_index(files: &[std::path::PathBuf]) -> PhpTypeIndex {
    use rayon::prelude::*;

    // ファイル毎の収集は独立なので並列化する。owner 重複時のマージ規則
    // (`merge_php_trait_use_entry`) が逐次実装と同じくファイル順で適用されるよう、
    // 順序保存の collect 後に入力順で統合する。
    let per_file: Vec<PhpTypeIndex> = files
        .par_iter()
        .map(|file_path| {
            let mut in_file = PhpTypeIndex::default();
            let Some(path) = file_path.to_str() else {
                return in_file;
            };
            let Ok(source) = parser::read_file(camino::Utf8Path::new(path)) else {
                return in_file;
            };
            let Ok(tree) = parser::parse_source(&source, LangId::Php) else {
                return in_file;
            };
            collect_php_types_from_node(tree.root_node(), source.as_bytes(), &mut in_file);
            in_file
        })
        .collect();

    let mut index = PhpTypeIndex::default();
    for file_index in per_file {
        for (owner, collected) in file_index.trait_uses {
            merge_php_trait_use_entry(&mut index.trait_uses, owner, collected);
        }
        for (name, decl) in file_index.decls {
            index.decls.entry(name).or_default().merge(decl);
        }
    }
    index
}

/// owner 単位の trait use 情報をマージ規則付きで登録する。
/// 同名 owner の再定義 (同一ファイル内・ファイル間とも) は traits を合算し、
/// declared_methods は先着を保持したまま ambiguous に倒す。
fn merge_php_trait_use_entry(
    uses_by_owner: &mut HashMap<String, PhpTraitUses>,
    owner: String,
    collected: PhpTraitUses,
) {
    if collected.traits.is_empty() && !collected.has_adaptation && !collected.ambiguous {
        return;
    }
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

fn collect_php_types_from_node(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    types: &mut PhpTypeIndex,
) {
    // enum (PHP 8.1+) も trait を use できるため収集対象に含める
    // (name/body フィールドは class と同形)。interface は trait を use できないので
    // 宣言の事実 (`decls`) だけを集める。
    if is_php_type_declaration(node.kind())
        && let Some(owner) = node
            .child_by_field_name("name")
            .and_then(|name| php_node_key(name, source))
        && let Some(body) = node.child_by_field_name("body")
    {
        let mut decl = PhpTypeDecl {
            count: 1,
            has_parent: {
                let mut cursor = node.walk();
                node.named_children(&mut cursor)
                    .any(|c| c.kind() == "base_clause")
            },
            concrete_methods: HashSet::new(),
        };
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
                let method_name = declaration
                    .child_by_field_name("name")
                    .and_then(|name| php_node_key(name, source));
                if !is_abstract && let Some(method_name) = method_name {
                    if declaration.child_by_field_name("body").is_some() {
                        decl.concrete_methods.insert(method_name.clone());
                    }
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
        types.decls.entry(owner.clone()).or_default().merge(decl);
        if node.kind() != "interface_declaration" {
            merge_php_trait_use_entry(&mut types.trait_uses, owner, collected);
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_php_types_from_node(child, source, types);
    }
}

/// 全 duplicate set を、ファイル毎 1 回の read + parse で並列に集計する。
///
/// 旧 `analyze_php_duplicate_set` は set 毎に全ファイルを read + parse していた。
/// ループを反転し、1 ファイルを 1 回だけ parse + alias 収集して出現する全 set を解析
/// する。旧実装の「ambiguous 確定後は残りファイルを見ない」短絡は worker 局所の
/// ambiguous フラグでファイル単位に再現する (結果は同一で走査量だけ最大 worker 数分
/// 増える)。集計は可換なので rayon の fold/reduce 順に依らず結果は決定的。
fn analyze_php_sets_over_files<F>(
    sets: &[(&str, HashSet<String>)],
    files: &[std::path::PathBuf],
    types: &PhpTypeIndex,
    is_test_path: &F,
) -> Vec<SetAccum>
where
    F: Fn(&Path) -> bool + Sync,
{
    use rayon::prelude::*;

    if sets.is_empty() {
        return Vec::new();
    }

    files
        .par_iter()
        .fold(
            || vec![SetAccum::default(); sets.len()],
            |mut acc, file_path| {
                analyze_php_file_into(sets, file_path, types, is_test_path, &mut acc);
                acc
            },
        )
        .reduce(
            || vec![SetAccum::default(); sets.len()],
            |mut merged, local| {
                for (m, l) in merged.iter_mut().zip(local) {
                    m.merge(l);
                }
                merged
            },
        )
}

/// 1 ファイルを解析して該当する全 set の accum を更新する。parse と alias 収集は
/// 最初に必要になった set の時点で 1 回だけ行い、以降の set は同じ結果を使い回す。
fn analyze_php_file_into<F>(
    sets: &[(&str, HashSet<String>)],
    file_path: &Path,
    types: &PhpTypeIndex,
    is_test_path: &F,
    acc: &mut [SetAccum],
) where
    F: Fn(&Path) -> bool,
{
    let Some(path_str) = file_path.to_str() else {
        return;
    };
    let Ok(source) = parser::read_file(camino::Utf8Path::new(path_str)) else {
        return;
    };

    let mut parsed: Option<Option<(tree_sitter::Tree, PhpFileAliases)>> = None;
    let mut is_test: Option<bool> = None;

    for ((bare_key, owners), set_acc) in sets.iter().zip(acc.iter_mut()) {
        // この worker で既に ambiguous 確定した set は解析しない (sticky のため結果不変)。
        if set_acc.ambiguous {
            continue;
        }
        // `__construct` set の参照源は `new Foo()` で、ソースに `__construct` 文字列が
        // 現れないため bare 名の prefilter では素通りできない。`new` を含むファイルも
        // parse 対象に残す (過剰マッチは parse が走るだけで無害)。
        if !contains_ascii_case_insensitive(source.as_bytes(), bare_key.as_bytes())
            && !(*bare_key == "__construct"
                && contains_ascii_case_insensitive(source.as_bytes(), b"new"))
        {
            continue;
        }
        let file_parse = parsed.get_or_insert_with(|| {
            parser::parse_source(&source, LangId::Php).ok().map(|tree| {
                let aliases = collect_php_file_aliases(tree.root_node(), source.as_bytes());
                (tree, aliases)
            })
        });
        let Some((tree, aliases)) = file_parse else {
            // parse 失敗は 1 回で確定し、旧実装の per-set continue と同じく
            // このファイル全体をスキップする。
            return;
        };
        let mut analysis = PhpFileAnalysis::default();
        visit_php_node(
            tree.root_node(),
            source.as_bytes(),
            owners,
            bare_key,
            None,
            types,
            aliases,
            &mut analysis,
        );
        if analysis.ambiguous {
            set_acc.ambiguous = true;
            continue;
        }
        if analysis.scoped_counts.is_empty() {
            continue;
        }
        let is_test = *is_test.get_or_insert_with(|| is_test_path(file_path));
        for (owner, count) in analysis.scoped_counts {
            let entry = set_acc.counts.entry(owner).or_insert((0, 0));
            if is_test {
                entry.1 = entry.1.saturating_add(count);
            } else {
                entry.0 = entry.0.saturating_add(count);
            }
        }
    }
}

#[derive(Default)]
struct PhpFileAnalysis {
    scoped_counts: HashMap<String, usize>,
    ambiguous: bool,
}

/// ファイル単位の `use X\Y as Z;` alias 情報。
///
/// - `resolved`: alias 名 (folded) → 対象クラス末尾名 (folded)。単一 namespace かつ
///   alias 名の競合が無く一意解決できた場合のみ `Some`。
/// - `alias_names`: ファイル内に現れた全 use 名 (folded、implicit alias 含む)。
///   `resolved` が `None` (multi-namespace 等で不完全) のとき、alias 名への scoped call
///   を Ambiguous に倒す判定に使う (silent Ignore による dead 誤検出を防ぐ)。
#[derive(Default)]
struct PhpFileAliases {
    resolved: Option<HashMap<String, String>>,
    alias_names: HashSet<String>,
}

/// ファイル内の `use` 宣言 (grouped 含む) から alias マップを収集する。
/// PSR-4 の 1 ファイル 1 namespace ではファイル全体マップで正しく解決できる。
/// namespace ブロックが複数ある場合は scope 追跡をせず `resolved: None` に倒す。
fn collect_php_file_aliases(root: tree_sitter::Node<'_>, source: &[u8]) -> PhpFileAliases {
    let mut map: HashMap<String, String> = HashMap::new();
    let mut alias_names: HashSet<String> = HashSet::new();
    let mut conflicted = false;
    let mut namespace_count = 0usize;
    collect_php_aliases_from_node(
        root,
        source,
        &mut map,
        &mut alias_names,
        &mut conflicted,
        &mut namespace_count,
    );
    let resolved = if conflicted || namespace_count > 1 {
        None
    } else {
        Some(map)
    };
    PhpFileAliases {
        resolved,
        alias_names,
    }
}

fn collect_php_aliases_from_node(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    map: &mut HashMap<String, String>,
    alias_names: &mut HashSet<String>,
    conflicted: &mut bool,
    namespace_count: &mut usize,
) {
    match node.kind() {
        "namespace_definition" => *namespace_count += 1,
        "namespace_use_declaration" => {
            collect_php_use_clause_aliases(node, source, map, alias_names, conflicted);
            // use 文の中はこれ以上潜らない (clause は上で処理済み)。
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_php_aliases_from_node(child, source, map, alias_names, conflicted, namespace_count);
    }
}

/// `namespace_use_declaration` 配下の clause (grouped use 含む) から
/// alias 名 → 対象末尾クラス名を登録する。同一 alias 名の再定義は競合として記録する。
fn collect_php_use_clause_aliases(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    map: &mut HashMap<String, String>,
    alias_names: &mut HashSet<String>,
    conflicted: &mut bool,
) {
    if node.kind() == "namespace_use_clause" {
        let mut cursor = node.walk();
        let named: Vec<tree_sitter::Node<'_>> = node.named_children(&mut cursor).collect();
        // clause = [target(qualified_name|name), alias(name)?]
        let Some(target) = named.first() else {
            return;
        };
        let Some(target_key) = php_node_key(*target, source) else {
            *conflicted = true;
            return;
        };
        let alias_key = named
            .get(1)
            .and_then(|alias| php_node_key(*alias, source))
            .unwrap_or_else(|| target_key.clone());
        alias_names.insert(alias_key.clone());
        if map.insert(alias_key, target_key).is_some() {
            *conflicted = true;
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_php_use_clause_aliases(child, source, map, alias_names, conflicted);
    }
}

/// `self::` / `new self()` 解決に使う enclosing type 情報。trait 本体内の `self` は
/// 合成先ホストの文脈で解決される (PHP 意味論) ため、名前に加えて宣言種別
/// (trait か否か) も伝播する (GitLab #34)。
struct PhpEnclosingType {
    name: String,
    is_trait: bool,
}

#[expect(clippy::too_many_arguments)]
fn visit_php_node(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    owners: &HashSet<String>,
    bare_key: &str,
    current_type: Option<&PhpEnclosingType>,
    types: &PhpTypeIndex,
    aliases: &PhpFileAliases,
    analysis: &mut PhpFileAnalysis,
) {
    if analysis.ambiguous {
        return;
    }

    let current_type_buf = php_class_context_for_node(node, source, owners, bare_key, types);
    let next_type = current_type_buf.as_ref().or(current_type);

    process_php_liveness_node(
        node, source, owners, bare_key, next_type, types, aliases, analysis,
    );

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit_php_node(
            child, source, owners, bare_key, next_type, types, aliases, analysis,
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
    types: &PhpTypeIndex,
) -> Option<PhpEnclosingType> {
    if !is_php_type_declaration(node.kind()) {
        return None;
    }
    // trait 判定は宣言ノード種別で行う。`types.trait_uses.contains_key()` は「別 trait を
    // use しない trait」を含まず「trait を use する class/enum」を含むため使えない。
    let is_trait = node.kind() == "trait_declaration";
    node.child_by_field_name("name")
        .and_then(|name| php_node_key(name, source))
        .filter(|key| {
            !matches!(
                php_resolve_trait_dispatch(key, bare_key, owners, &types.trait_uses),
                PhpOwnerResolution::Ignore
            )
        })
        .map(|name| PhpEnclosingType { name, is_trait })
}

#[expect(clippy::too_many_arguments)]
fn process_php_liveness_node(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    owners: &HashSet<String>,
    bare_key: &str,
    current_type: Option<&PhpEnclosingType>,
    types: &PhpTypeIndex,
    aliases: &PhpFileAliases,
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
                    current_type,
                    types,
                    aliases,
                    analysis,
                );
            }
        }
        // `__construct` の参照源はメソッド呼び出しではなく `new Foo()`。
        // constructor set のときだけ object creation をクラス名票として数える。
        "object_creation_expression" => {
            if bare_key == "__construct" {
                record_php_object_creation(
                    node,
                    source,
                    owners,
                    bare_key,
                    current_type,
                    types,
                    aliases,
                    analysis,
                );
            }
        }
        // `$x->m()` / `$x?->m()` (PHP 8 nullsafe) は receiver の型を静的に辿れない。
        // nullsafe を数え漏らすと、`?->` でしか呼ばれない同名メソッドが両方 dead に出る。
        "member_call_expression" | "nullsafe_member_call_expression" => {
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

#[expect(clippy::too_many_arguments)]
fn record_php_scoped_call(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    owners: &HashSet<String>,
    bare_key: &str,
    current_type: Option<&PhpEnclosingType>,
    types: &PhpTypeIndex,
    aliases: &PhpFileAliases,
    analysis: &mut PhpFileAnalysis,
) {
    match php_scoped_call_owner(node, source, owners, bare_key, current_type, types, aliases) {
        PhpOwnerResolution::Resolved(owner) => {
            *analysis.scoped_counts.entry(owner).or_default() += 1;
        }
        PhpOwnerResolution::Ambiguous => analysis.ambiguous = true,
        PhpOwnerResolution::Ignore => {}
    }
}

/// `new Foo()` を `Foo::__construct` への確定参照として数える。
/// `new self()` は enclosing class、`new static()` / `new parent()` / `new $var()` は
/// Ambiguous、anonymous class は `php_resolve_anonymous_class_creation` で解決する。
#[expect(clippy::too_many_arguments)]
fn record_php_object_creation(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    owners: &HashSet<String>,
    bare_key: &str,
    current_type: Option<&PhpEnclosingType>,
    types: &PhpTypeIndex,
    aliases: &PhpFileAliases,
    analysis: &mut PhpFileAnalysis,
) {
    let Some(target) = node.named_child(0) else {
        return;
    };
    let resolution = match target.kind() {
        "name" | "qualified_name" => {
            let Some(folded) = php_node_key(target, source) else {
                return;
            };
            php_resolve_scope_name(
                &folded,
                bare_key,
                owners,
                current_type,
                types,
                aliases,
                PhpScopeUse::ObjectCreation,
            )
        }
        // `new $cls()` は動的クラス名で owner を静的解決できない。
        "variable_name" => PhpOwnerResolution::Ambiguous,
        "anonymous_class" => php_resolve_anonymous_class_creation(
            target,
            source,
            owners,
            bare_key,
            current_type,
            types,
            aliases,
        ),
        _ => PhpOwnerResolution::Ignore,
    };
    match resolution {
        PhpOwnerResolution::Resolved(owner) => {
            *analysis.scoped_counts.entry(owner).or_default() += 1;
        }
        PhpOwnerResolution::Ambiguous => analysis.ambiguous = true,
        PhpOwnerResolution::Ignore => {}
    }
}

/// `new class(...) extends Base { ... }` (無名クラスの生成) が呼ぶ constructor を解決する。
///
/// 無名クラス自身は名前を持たず候補にならない。自前の `__construct` を宣言していれば
/// それが呼ばれるので候補へは届かない (中の `parent::__construct()` は scoped call として
/// 別途 Ambiguous に倒れる)。宣言していなければ constructor は合成した trait か継承元から
/// 来る — trait は合成先を辿らず Ambiguous、`extends Base` は `new Base()` と同じ解決に
/// 委ねる。旧実装は無名クラスを一律 Ignore にしていたため、`new class extends Base {}`
/// でしか生成されない `Base.__construct` が dead と誤報されていた。
fn php_resolve_anonymous_class_creation(
    anon: tree_sitter::Node<'_>,
    source: &[u8],
    owners: &HashSet<String>,
    bare_key: &str,
    current_type: Option<&PhpEnclosingType>,
    types: &PhpTypeIndex,
    aliases: &PhpFileAliases,
) -> PhpOwnerResolution {
    let mut declares_ctor = false;
    let mut uses_trait = false;
    if let Some(body) = anon.child_by_field_name("body") {
        let mut cursor = body.walk();
        for decl in body.named_children(&mut cursor) {
            match decl.kind() {
                "method_declaration" => {
                    declares_ctor |= decl
                        .child_by_field_name("name")
                        .and_then(|name| php_node_key(name, source))
                        .is_some_and(|key| key == bare_key);
                }
                "use_declaration" => uses_trait = true,
                _ => {}
            }
        }
    }
    if declares_ctor {
        return PhpOwnerResolution::Ignore;
    }
    if uses_trait {
        return PhpOwnerResolution::Ambiguous;
    }
    let mut cursor = anon.walk();
    let base = anon
        .named_children(&mut cursor)
        .find(|c| c.kind() == "base_clause")
        .and_then(|clause| clause.named_child(0));
    let Some(base) = base else {
        // 継承元も trait も無ければ constructor は暗黙の既定のみ。
        return PhpOwnerResolution::Ignore;
    };
    match php_node_key(base, source) {
        Some(folded) => php_resolve_scope_name(
            &folded,
            bare_key,
            owners,
            current_type,
            types,
            aliases,
            PhpScopeUse::ObjectCreation,
        ),
        None => PhpOwnerResolution::Ambiguous,
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
    current_type: Option<&PhpEnclosingType>,
    types: &PhpTypeIndex,
    aliases: &PhpFileAliases,
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
    if text.starts_with('$') {
        return PhpOwnerResolution::Ambiguous;
    }
    let folded = php_fold_name(
        text.trim_start_matches('\\')
            .rsplit('\\')
            .next()
            .unwrap_or(text),
    );
    php_resolve_scope_name(
        &folded,
        bare_key,
        owners,
        current_type,
        types,
        aliases,
        PhpScopeUse::StaticCall,
    )
}

/// scope 名が現れた構文。候補へ到達し得る経路が違うため、候補にも trait 合成経由の
/// 候補にも辿り着かない scope の扱いを分ける (`php_resolve_unowned_scope`)。
#[derive(Clone, Copy)]
enum PhpScopeUse {
    /// `X::m()`
    StaticCall,
    /// `new X()` (`__construct` set のみ)
    ObjectCreation,
}

/// scope 名 (folded 済み) を candidate owner へ解決する。scoped call (`X::m()`) と
/// object creation (`new X()`) で共用する。
fn php_resolve_scope_name(
    folded: &str,
    bare_key: &str,
    owners: &HashSet<String>,
    current_type: Option<&PhpEnclosingType>,
    types: &PhpTypeIndex,
    aliases: &PhpFileAliases,
    scope_use: PhpScopeUse,
) -> PhpOwnerResolution {
    match folded {
        // trait 本体内の `self::` / `new self()` は合成先ホストの文脈で解決され、
        // ホスト側 (または解決順で優先される別合成 trait) の同名 override が呼ばれ得る。
        // 定義元 trait への確定票にすると override 側が参照 0 件になり dead 誤検出する
        // (GitLab #34) ため Ambiguous に倒し、duplicate set 全体を旧スキップへ
        // フォールバックさせる。class/enum 本体の `self` は宣言クラスへ静的束縛される
        // ため従来どおり確定解決する (LSB は `static::` で別途 Ambiguous 済み)。
        "self" => match current_type {
            Some(ctx) if ctx.is_trait => PhpOwnerResolution::Ambiguous,
            Some(ctx) => php_resolve_trait_dispatch(&ctx.name, bare_key, owners, &types.trait_uses),
            None => PhpOwnerResolution::Ambiguous,
        },
        // `static::` は遅延静的束縛 (late static binding) でサブクラス override へ
        // ディスパッチされ得る。継承グラフを持たない本解析では enclosing class へ
        // 確定解決するとサブクラス側メソッドの dead 誤検出になるため `parent::` と
        // 同じく Ambiguous に倒す (duplicate set 全体が旧スキップへフォールバック)。
        "parent" | "static" => PhpOwnerResolution::Ambiguous,
        _ if owners.contains(folded) => PhpOwnerResolution::Resolved(folded.to_string()),
        _ => {
            // `use X\Y as Z; Z::m()` / `new Z()` の alias を実クラス名へ解決してから
            // trait dispatch を含む通常解決へ流す。alias マップが不完全 (multi-namespace /
            // 競合) な場合、alias 名への参照だけ Ambiguous に倒す (silent Ignore による
            // dead 誤検出を防ぐ)。
            let target = match &aliases.resolved {
                Some(map) => map.get(folded).map_or(folded, String::as_str),
                None if aliases.alias_names.contains(folded) => {
                    return PhpOwnerResolution::Ambiguous;
                }
                None => folded,
            };
            match php_resolve_trait_dispatch(target, bare_key, owners, &types.trait_uses) {
                PhpOwnerResolution::Ignore => {
                    php_resolve_unowned_scope(target, bare_key, scope_use, &types.decls)
                }
                resolved => resolved,
            }
        }
    }
}

/// 候補 owner にも trait 合成経由の候補にも辿り着かなかった scope クラス `X` について、
/// `X::m()` / `new X()` が候補メソッドへ到達し得るかを判定する。
///
/// 旧実装はこの場合を一律 `Ignore` (票を捨てる) にしていたため、`class Child extends Base {}`
/// に対する `Child::make()` / `new Child()` が継承元 `Base` の候補へ届かず、`Base.make` /
/// `Base.__construct` が参照 0 件で dead と誤報されていた (TS 側は同条件を Ambiguous に
/// 倒している)。継承グラフを辿って票の行き先を推測はせず、候補へ届かないと言い切れる
/// 場合だけ `Ignore` に残す:
///
/// - `X` が走査対象内に 1 つだけ宣言され、対象メソッドを実装付きで自己宣言している →
///   PHP の解決順 (自クラス > trait > 親) で自クラスのメソッドへ静的に解決される。
/// - 静的呼び出し (`X::m()`) のそれ以外 → `Ambiguous`。親からの継承に加え、走査対象外の
///   クラス (Laravel の facade 等) は `__callStatic` で任意のインスタンスへ転送し得る。
/// - `new X()` のそれ以外 → `X` が走査対象内で `extends` を持つ (または同名宣言が複数ある)
///   なら親の constructor を継承し得るので `Ambiguous`。`extends` を持たない宣言と
///   走査対象外 (vendor / 組み込み) のクラスは `Ignore` — constructor は `__callStatic`
///   のような転送を受けず、依存先のクラスがリポジトリ内のクラスを継承することもない
///   (`new \Exception()` のたびに constructor の duplicate set 全体が Ambiguous へ倒れ、
///   未使用 constructor を 1 つも検出できなくなるのを避ける)。
fn php_resolve_unowned_scope(
    target: &str,
    bare_key: &str,
    scope_use: PhpScopeUse,
    decls: &HashMap<String, PhpTypeDecl>,
) -> PhpOwnerResolution {
    let decl = decls.get(target);
    if decl.is_some_and(|d| d.count == 1 && d.concrete_methods.contains(bare_key)) {
        return PhpOwnerResolution::Ignore;
    }
    match scope_use {
        PhpScopeUse::StaticCall => PhpOwnerResolution::Ambiguous,
        PhpScopeUse::ObjectCreation => match decl {
            Some(d) if d.count != 1 || d.has_parent => PhpOwnerResolution::Ambiguous,
            _ => PhpOwnerResolution::Ignore,
        },
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
    let Some(owner) = matching.into_iter().next() else {
        return PhpOwnerResolution::Ignore;
    };
    PhpOwnerResolution::Resolved(owner)
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
