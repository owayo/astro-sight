//! TS/JS と PHP の class member 単位の liveness 解析。
//!
//! 同名 bare member (例: `VoiceLogSettingModel.isOmnis` と `VoiceLogModel.isOmnis`) が
//! 複数 owner クラスに存在する場合、`build_dead_code_name_index` の `name_counts > 1`
//! 保守的スキップを **owner 一意推定できるケースに限り** 緩和するためのヘルパー。
//!
//! 適用範囲は TS/JS/TSX のみ。Python/Ruby は属性が動的、JVM 系は型解決なしだと
//! receiver 判定が中途半端、Rust は trait/impl の意味論が別物のため対象外。
//!
//! 推定ロジック:
//! - 候補ファイル内で duplicate owner のうち **1 つだけ** import + `.member`
//!   property access → その owner の live 票
//! - duplicate owner を複数 import + `.member` → ambiguous (旧スキップを維持)
//! - owner import なし + `.member` → ambiguous (旧スキップを維持)
//! - `obj["member"]` / `'member'` 文字列リテラル → ambiguous (computed access 可能性)
//! - 解析後、duplicate set 内で production 票 0 / test 票 > 0 のメンバーは TestOnly、
//!   production 票 0 / test 票 0 のメンバーは Dead、それ以外は Live を返す
//!
//! 推定不能な duplicate set は全 candidate に対して Ambiguous を返し、呼び出し側で
//! 旧スキップへフォールバックさせる。

use std::collections::{HashMap, HashSet};
use std::path::Path;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Query, QueryCursor};

use crate::engine::parser;
use crate::engine::refs;
use crate::language::LangId;

/// duplicate な同名 TS/JS class member の liveness 判定結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemberStatus {
    /// 一意推定で live (≥1 ファイルで唯一の owner として import + property access あり)。
    Live,
    /// production では参照 0、test ファイルのみで参照あり → test_only 分類。
    TestOnly,
    /// production / test ともに参照 0 → dead 分類。
    Dead,
    /// 推定不能 (旧 `name_counts > 1` スキップへフォールバック)。
    Ambiguous,
}

/// TS/JS class member の owner-aware liveness インデックス。
///
/// key = (file_rel, owner_class, bare_member)。dead-code 検出側で
/// `JsTsMemberLiveness::status_for` を呼び出して `MemberStatus` を取得する。
#[derive(Debug, Default)]
pub(crate) struct JsTsMemberLiveness {
    statuses: HashMap<(String, String, String), MemberStatus>,
}

impl JsTsMemberLiveness {
    /// 候補シンボル群から duplicate な TS/JS class member の owner-aware liveness を解析する。
    ///
    /// `candidates` は `(name, kind, file_rel, lang)` のタプル列。`name` は qualname 形式
    /// (`Container.member`)。`canonical_dir` は走査対象のルート (canonicalize 済み)。
    /// `is_test_path` は test ディレクトリ判定。
    pub(crate) fn build<F>(
        candidates: &[(String, String, String, LangId)],
        canonical_dir: &Path,
        is_test_path: F,
    ) -> Self
    where
        F: Fn(&Path) -> bool,
    {
        let mut statuses: HashMap<(String, String, String), MemberStatus> = HashMap::new();

        // Step 1: TS/JS class member candidate を集める。
        let mut js_ts_members: Vec<MemberCandidate> = Vec::new();
        for (name, kind, file, lang) in candidates {
            if !is_js_ts_lang(*lang) {
                continue;
            }
            if !is_class_member_kind(kind) {
                continue;
            }
            let Some((owner, bare)) = name.rsplit_once('.') else {
                continue;
            };
            // 多段 qualname (例: namespace.Class.member) は class.member とみなさない。
            if owner.contains('.') {
                continue;
            }
            js_ts_members.push(MemberCandidate {
                owner: owner.to_string(),
                bare: bare.to_string(),
                file: file.clone(),
            });
        }
        if js_ts_members.is_empty() {
            return Self { statuses };
        }

        // Step 2: bare name 単位で duplicate set を構築。
        let mut bare_to_members: HashMap<&str, Vec<&MemberCandidate>> = HashMap::new();
        for m in &js_ts_members {
            bare_to_members.entry(m.bare.as_str()).or_default().push(m);
        }

        // Step 2.5: duplicate owner を持つ set がなければ全ファイル収集を skip して早期 return。
        let has_duplicate_set = bare_to_members.values().any(|v| {
            let owners: HashSet<&str> = v.iter().map(|m| m.owner.as_str()).collect();
            owners.len() >= 2
        });
        if !has_duplicate_set {
            return Self { statuses };
        }

        // Step 3: TS/JS ファイル一覧を収集 (一度だけ)。
        let Ok(all_files) = refs::collect_files(canonical_dir, None) else {
            return Self { statuses };
        };
        let ts_js_files: Vec<(std::path::PathBuf, LangId)> = all_files
            .into_iter()
            .filter_map(|p| {
                let s = p.to_str()?;
                let lang = LangId::from_path(camino::Utf8Path::new(s)).ok()?;
                if is_js_ts_lang(lang) {
                    Some((p, lang))
                } else {
                    None
                }
            })
            .collect();

        // Step 4: 各 duplicate set ごとに解析。
        for (bare, members) in &bare_to_members {
            // 同 owner で同 member 名を 2 回 export するケースは通常ないため、
            // owner 単位の uniq に正規化する。
            let owners: HashSet<&str> = members.iter().map(|m| m.owner.as_str()).collect();
            if owners.len() < 2 {
                // duplicate owner でない場合は対象外 (`name_counts > 1` も発生しない想定だが、
                // 同じ owner が複数ファイルに同名 export を持つ稀ケースは旧スキップに任せる)。
                continue;
            }

            let analysis = analyze_duplicate_set(
                bare,
                &owners,
                members,
                &ts_js_files,
                canonical_dir,
                &is_test_path,
            );

            match analysis {
                DuplicateSetResult::Ambiguous => {
                    for m in members {
                        statuses.insert(
                            (m.file.clone(), m.owner.clone(), (*bare).to_string()),
                            MemberStatus::Ambiguous,
                        );
                    }
                }
                DuplicateSetResult::Counted(counts) => {
                    for m in members {
                        let (prod, tst) = counts.get(m.owner.as_str()).copied().unwrap_or((0, 0));
                        let status = if prod > 0 {
                            MemberStatus::Live
                        } else if tst > 0 {
                            MemberStatus::TestOnly
                        } else {
                            MemberStatus::Dead
                        };
                        statuses.insert(
                            (m.file.clone(), m.owner.clone(), (*bare).to_string()),
                            status,
                        );
                    }
                }
            }
        }

        Self { statuses }
    }

    /// 指定 (file, owner, bare) の MemberStatus を返す。エントリがなければ None。
    pub(crate) fn status_for(&self, owner: &str, bare: &str, file: &str) -> Option<MemberStatus> {
        self.statuses
            .get(&(file.to_string(), owner.to_string(), bare.to_string()))
            .copied()
    }
}

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

        let Some(php_files) = collect_php_files(canonical_dir) else {
            return Self { statuses };
        };

        for (bare_key, members) in &bare_to_members {
            let owners: HashSet<String> = members.iter().map(|m| php_fold_name(&m.owner)).collect();
            if owners.len() < 2 {
                continue;
            }
            let analysis = analyze_php_duplicate_set(bare_key, &owners, &php_files, &is_test_path);

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
                        let status = if prod > 0 {
                            MemberStatus::Live
                        } else if tst > 0 {
                            MemberStatus::TestOnly
                        } else {
                            MemberStatus::Dead
                        };
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

fn collect_php_files(canonical_dir: &Path) -> Option<Vec<std::path::PathBuf>> {
    let files = refs::collect_files(canonical_dir, None).ok()?;
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

struct MemberCandidate {
    owner: String,
    bare: String,
    file: String,
}

enum DuplicateSetResult {
    /// 一意推定が成立し、owner 別の (prod, test) カウントを保持する。
    /// キーは owner 名 (String) で借用ライフタイムから独立させる。
    Counted(HashMap<String, (usize, usize)>),
    /// 推定不能。呼び出し側で旧スキップへフォールバックする。
    Ambiguous,
}

fn analyze_duplicate_set<F>(
    bare: &str,
    owners: &HashSet<&str>,
    members: &[&MemberCandidate],
    files: &[(std::path::PathBuf, LangId)],
    canonical_dir: &Path,
    is_test_path: F,
) -> DuplicateSetResult
where
    F: Fn(&Path) -> bool,
{
    let mut counts: HashMap<String, (usize, usize)> = HashMap::new();
    let owners_vec: Vec<&str> = owners.iter().copied().collect();

    for (file_path, lang) in files {
        let utf8 = match file_path.to_str() {
            Some(s) => camino::Utf8Path::new(s),
            None => continue,
        };
        let source = match parser::read_file(utf8) {
            Ok(s) => s,
            Err(_) => continue,
        };

        // バイトレベルで bare member が含まれないファイルは parse skip (高速化)。
        if memchr::memmem::find(source.as_bytes(), bare.as_bytes()).is_none() {
            continue;
        }

        // ambiguous source の早期検出 (string literal / computed access)。
        if contains_ambiguous_member_token(source.as_bytes(), bare) {
            return DuplicateSetResult::Ambiguous;
        }

        let tree = match parser::parse_source(&source, *lang) {
            Ok(t) => t,
            Err(_) => continue,
        };

        let analysis = analyze_file(
            tree.root_node(),
            source.as_bytes(),
            &owners_vec,
            bare,
            *lang,
        );

        if analysis.member_access_count == 0 {
            continue;
        }

        // duplicate owner のうち、この **ファイル内** で定義されている owner を集める。
        // 自ファイル内の `this.member` は `imported_owners` 推定だけで unrelated import 側へ
        // 誤帰属しうるため、自ファイル定義 owner を effective_owners に統合する
        // (codex review 指摘の FP 修正)。
        let file_rel = file_path
            .strip_prefix(canonical_dir)
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut local_defined: Vec<&str> = members
            .iter()
            .filter(|m| m.file == file_rel)
            .map(|m| m.owner.as_str())
            .collect();
        local_defined.sort();
        local_defined.dedup();

        let mut effective_owners: Vec<&str> = analysis.imported_owners.clone();
        for o in &local_defined {
            if !effective_owners.contains(o) {
                effective_owners.push(*o);
            }
        }

        match effective_owners.len() {
            1 => {
                let owner_key = effective_owners[0];
                let is_test = is_test_path(file_path.as_path());
                let entry = counts.entry(owner_key.to_string()).or_insert((0, 0));
                if is_test {
                    entry.1 = entry.1.saturating_add(analysis.member_access_count);
                } else {
                    entry.0 = entry.0.saturating_add(analysis.member_access_count);
                }
            }
            _ => {
                // imported_owners.len() == 0 (owner import なし、local 定義もなし)、
                // imported_owners.len() >= 2 (複数 owner import)、
                // local 定義 + unrelated owner import の混在、いずれも ambiguous。
                return DuplicateSetResult::Ambiguous;
            }
        }
    }

    DuplicateSetResult::Counted(counts)
}

/// ファイル内 1 つの bare member について import と property access を集計する。
struct FileAnalysis<'a> {
    /// 当該ファイル内で対象 owner のうちどれが import されているか (重複なし、入力順)。
    imported_owners: Vec<&'a str>,
    /// `obj.member` 形式の property access の出現回数 (`?.` 含む)。
    member_access_count: usize,
}

fn analyze_file<'a>(
    root: tree_sitter::Node<'_>,
    source: &[u8],
    target_owners: &[&'a str],
    target_member: &str,
    lang: LangId,
) -> FileAnalysis<'a> {
    let mut analysis = FileAnalysis {
        imported_owners: Vec::new(),
        member_access_count: 0,
    };

    // Owner import の集計。
    let owners_set: HashSet<&str> = target_owners.iter().copied().collect();
    let mut imported: HashSet<&str> = HashSet::new();
    let mut has_namespace_import = false;
    if let Some(language) = ts_language_for(lang)
        && let Ok(query) = Query::new(&language, IMPORT_QUERY)
    {
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, root, source);
        while let Some(m) = matches.next() {
            for cap in m.captures {
                let Ok(text) = cap.node.utf8_text(source) else {
                    continue;
                };
                let cap_name = &query.capture_names()[cap.index as usize];
                match *cap_name {
                    "default_name" | "import_name" => {
                        if let Some(&owner) = owners_set.get(text) {
                            imported.insert(owner);
                        }
                    }
                    "import_alias" => {
                        // alias の場合は元名ではなく alias の名前を見るが、aliased が owner の
                        // 元名を持つかは別 capture (import_name) を参照。alias を使うと
                        // ローカル名は alias になるため、property access の `obj` 側で
                        // alias 名が出てくる可能性があるが、owner 推定は import_name で行う。
                        // → alias は単独で owner 一致を判定しない。
                    }
                    "namespace_name" => {
                        // `import * as ns from ...` は ns.Foo 経由でアクセスされうるが、
                        // 静的に名前を解決できないため ambiguous へ倒す材料にする。
                        has_namespace_import = true;
                    }
                    _ => {}
                }
            }
        }
    }
    if has_namespace_import {
        // namespace import + duplicate owner は安全側で「複数 owner 同時 import 状態」と
        // みなすため、当該ファイルは imported_owners >= 2 相当とする。
        let mut owners_vec: Vec<&str> = target_owners.to_vec();
        owners_vec.sort();
        owners_vec.dedup();
        analysis.imported_owners = owners_vec;
    } else {
        // 元順 (target_owners 順) で安定化。
        for &o in target_owners {
            if imported.contains(o) {
                analysis.imported_owners.push(o);
            }
        }
    }

    // property access (`obj.member` / `obj?.member`) の集計。
    if let Some(language) = ts_language_for(lang)
        && let Ok(query) = Query::new(&language, MEMBER_ACCESS_QUERY)
    {
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, root, source);
        while let Some(m) = matches.next() {
            for cap in m.captures {
                let Ok(text) = cap.node.utf8_text(source) else {
                    continue;
                };
                if text == target_member {
                    analysis.member_access_count = analysis.member_access_count.saturating_add(1);
                }
            }
        }
    }

    analysis
}

/// 推定不能な access (computed property / 文字列リテラル) が含まれるかをバイトレベルで検出する。
fn contains_ambiguous_member_token(source: &[u8], bare: &str) -> bool {
    let dq = format!("\"{bare}\"");
    let sq = format!("'{bare}'");
    let bt = format!("`{bare}`");
    memchr::memmem::find(source, dq.as_bytes()).is_some()
        || memchr::memmem::find(source, sq.as_bytes()).is_some()
        || memchr::memmem::find(source, bt.as_bytes()).is_some()
}

fn analyze_php_duplicate_set<F>(
    bare_key: &str,
    owners: &HashSet<String>,
    files: &[std::path::PathBuf],
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
    analysis: &mut PhpFileAnalysis,
) {
    if analysis.ambiguous {
        return;
    }

    let current_class_buf = php_class_context_for_node(node, source, owners);
    let next_class = current_class_buf.as_deref().or(current_class);

    process_php_liveness_node(node, source, owners, bare_key, next_class, analysis);

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit_php_node(child, source, owners, bare_key, next_class, analysis);
        if analysis.ambiguous {
            break;
        }
    }
}

fn php_class_context_for_node(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    owners: &HashSet<String>,
) -> Option<String> {
    if !is_php_type_declaration(node.kind()) {
        return None;
    }
    node.child_by_field_name("name")
        .and_then(|name| php_node_key(name, source))
        .filter(|key| owners.contains(key))
}

fn process_php_liveness_node(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    owners: &HashSet<String>,
    bare_key: &str,
    current_class: Option<&str>,
    analysis: &mut PhpFileAnalysis,
) {
    match node.kind() {
        "scoped_call_expression" => {
            if php_call_name_matches(node, source, bare_key) {
                record_php_scoped_call(node, source, owners, current_class, analysis);
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
    current_class: Option<&str>,
    analysis: &mut PhpFileAnalysis,
) {
    match php_scoped_call_owner(node, source, owners, current_class) {
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
    current_class: Option<&str>,
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
        "self" | "static" => current_class
            .map(|owner| PhpOwnerResolution::Resolved(owner.to_string()))
            .unwrap_or(PhpOwnerResolution::Ambiguous),
        "parent" => PhpOwnerResolution::Ambiguous,
        _ if owners.contains(&folded) => PhpOwnerResolution::Resolved(folded),
        _ if text.starts_with('$') => PhpOwnerResolution::Ambiguous,
        _ => PhpOwnerResolution::Ignore,
    }
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

fn ts_language_for(lang: LangId) -> Option<tree_sitter::Language> {
    if lang.is_lexer_only() {
        return None;
    }
    Some(lang.ts_language())
}

fn is_js_ts_lang(lang: LangId) -> bool {
    matches!(lang, LangId::Typescript | LangId::Javascript | LangId::Tsx)
}

fn is_class_member_kind(kind: &str) -> bool {
    matches!(
        kind,
        "method" | "field" | "property" | "getter" | "setter" | "accessor"
    )
}

/// TS/JS class member 用 query。getter/setter/method/field を一括で抽出するため、
/// `import_specifier` の `name` / `alias` / namespace を細かく分けて capture する。
const IMPORT_QUERY: &str = r#"
(import_statement
  (import_clause
    [
      (identifier) @default_name
      (named_imports
        (import_specifier
          name: (identifier) @import_name
          alias: (identifier)? @import_alias))
      (namespace_import
        (identifier) @namespace_name)
    ]))
"#;

/// `obj.member` および `obj?.member` の右辺 property_identifier を捕捉する query。
/// shorthand `{ member }` や `obj["member"]` は対象外 (別途 ambiguous 検出)。
const MEMBER_ACCESS_QUERY: &str = r#"
(member_expression
  property: (property_identifier) @prop)
"#;
