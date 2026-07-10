use std::collections::{HashMap, HashSet};
use std::path::Path;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Query, QueryCursor};

use crate::engine::parser;
use crate::language::LangId;

use super::{
    DuplicateSetResult, MemberCandidate, MemberStatus, collect_source_files, is_class_member_kind,
    status_from_counts,
};

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
    /// `extra_files` は hidden ディレクトリ配下などの walk 対象外でも候補になった
    /// diff 由来ファイル (count 経路と走査集合を一致させる)。`is_test_path` は
    /// test ディレクトリ判定。
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

        // Step 3: TS/JS ファイル一覧を収集 (一度だけ)。hidden 配下の diff 候補も合流させる。
        let Some(all_files) = collect_source_files(canonical_dir, extra_files) else {
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
                        let status = status_from_counts(prod, tst);
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

fn ts_language_for(lang: LangId) -> Option<tree_sitter::Language> {
    if lang.is_lexer_only() {
        return None;
    }
    Some(lang.ts_language())
}

fn is_js_ts_lang(lang: LangId) -> bool {
    matches!(lang, LangId::Typescript | LangId::Javascript | LangId::Tsx)
}

/// import 文から owner 名を検出する query。
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
