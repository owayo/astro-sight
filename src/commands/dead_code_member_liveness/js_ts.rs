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

            let analysis = analyze_duplicate_set(bare, &owners, &ts_js_files, &is_test_path);

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
    files: &[(std::path::PathBuf, LangId)],
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

        // receiver を owner へ静的に辿れない access (factory / DI / 関数戻り値・
        // 引数経由のインスタンス等) が 1 件でもあれば、票の行き先を import 有無で
        // 推測せず duplicate set 全体を Ambiguous に倒す (旧スキップへフォールバック)。
        if analysis.has_unresolved_access {
            return DuplicateSetResult::Ambiguous;
        }
        if analysis.resolved_counts.is_empty() {
            continue;
        }

        let is_test = is_test_path(file_path.as_path());
        for (owner, count) in analysis.resolved_counts {
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

/// ファイル内 1 つの bare member について receiver 解決済み access を集計する。
struct FileAnalysis {
    /// receiver が owner へ静的に辿れた access の owner 別出現回数 (production/test 区別前)。
    resolved_counts: HashMap<String, usize>,
    /// receiver を owner へ静的に辿れない access があったか。
    has_unresolved_access: bool,
}

/// ローカル変数 / パラメータの owner 束縛。
enum VarBinding {
    /// `const a = new Alpha()` / `a: Alpha` のように単一 owner へ辿れる。
    Owner(String),
    /// `const xs: Alpha[] = ...` のように owner の配列型。`xs[i].member` で owner に辿れる。
    ElementOwner(String),
    /// factory 戻り値・非 owner 型・同名バインディング複数などで辿れない。
    Unresolvable,
}

fn analyze_file(
    root: tree_sitter::Node<'_>,
    source: &[u8],
    target_owners: &[&str],
    target_member: &str,
    lang: LangId,
) -> FileAnalysis {
    let mut analysis = FileAnalysis {
        resolved_counts: HashMap::new(),
        has_unresolved_access: false,
    };
    let owners_set: HashSet<&str> = target_owners.iter().copied().collect();

    // 1. クラス名として使えるローカル名 → owner のマップを作る。
    //    - import (default / named / alias): `import { Alpha as A }` → A → Alpha
    //    - 同ファイル内 class 定義: class Alpha {} → Alpha → Alpha
    //    `import * as ns` 経由のアクセス (`ns.Alpha.fmt()`) は receiver が
    //    member_expression になり通常の分類で不解決に落ちるため、namespace import の
    //    存在だけでファイル全体を Ambiguous にはしない (対象 member への access が
    //    無いファイルまで巻き込むのを避ける)。
    let mut local_to_owner: HashMap<String, String> = HashMap::new();
    if let Some(language) = ts_language_for(lang)
        && let Ok(query) = Query::new(&language, IMPORT_QUERY)
    {
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, root, source);
        while let Some(m) = matches.next() {
            let mut import_name: Option<&str> = None;
            let mut import_alias: Option<&str> = None;
            for cap in m.captures {
                let Ok(text) = cap.node.utf8_text(source) else {
                    continue;
                };
                let cap_name = &query.capture_names()[cap.index as usize];
                match *cap_name {
                    "default_name" => {
                        if owners_set.contains(text) {
                            local_to_owner.insert(text.to_string(), text.to_string());
                        }
                    }
                    "import_name" => import_name = Some(text),
                    "import_alias" => import_alias = Some(text),
                    _ => {}
                }
            }
            if let Some(name) = import_name
                && owners_set.contains(name)
            {
                let local = import_alias.unwrap_or(name);
                local_to_owner.insert(local.to_string(), name.to_string());
            }
        }
    }
    collect_local_class_definitions(root, source, &owners_set, &mut local_to_owner);

    // 2. ローカル変数 / パラメータの owner 束縛マップ。
    let mut var_bindings: HashMap<String, VarBinding> = HashMap::new();
    collect_var_bindings(
        root,
        source,
        &local_to_owner,
        &owners_set,
        &mut var_bindings,
    );

    // 3. `obj.member` / `obj?.member` の receiver を分類して票を入れる。
    if let Some(language) = ts_language_for(lang)
        && let Ok(query) = Query::new(&language, MEMBER_ACCESS_QUERY)
    {
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, root, source);
        while let Some(m) = matches.next() {
            for cap in m.captures {
                let node = cap.node;
                let Ok(text) = node.utf8_text(source) else {
                    continue;
                };
                if text != target_member {
                    continue;
                }
                let Some(member_expr) = node.parent() else {
                    continue;
                };
                match resolve_member_receiver(
                    member_expr,
                    source,
                    &local_to_owner,
                    &var_bindings,
                    &owners_set,
                ) {
                    Some(owner) => {
                        *analysis.resolved_counts.entry(owner).or_default() += 1;
                    }
                    None => {
                        analysis.has_unresolved_access = true;
                        return analysis;
                    }
                }
            }
        }
    }

    analysis
}

/// 同ファイル内の class 定義名 (owner に一致するもの) をクラス名マップへ加える。
fn collect_local_class_definitions(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    owners_set: &HashSet<&str>,
    local_to_owner: &mut HashMap<String, String>,
) {
    if matches!(node.kind(), "class_declaration" | "class")
        && let Some(name) = node.child_by_field_name("name")
        && let Ok(text) = name.utf8_text(source)
        && owners_set.contains(text)
    {
        local_to_owner.insert(text.to_string(), text.to_string());
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_local_class_definitions(child, source, owners_set, local_to_owner);
    }
}

/// binding マップへ 1 件登録する。同名バインディングが複数回現れた場合は
/// スコープ解決をせず `Unresolvable` に落とす (shadow / 別スコープの一意解決を諦める)。
fn register_binding(bindings: &mut HashMap<String, VarBinding>, name: String, value: VarBinding) {
    use std::collections::hash_map::Entry;
    match bindings.entry(name) {
        Entry::Vacant(e) => {
            e.insert(value);
        }
        Entry::Occupied(mut e) => {
            e.insert(VarBinding::Unresolvable);
        }
    }
}

/// binding パターン (identifier / destructuring / rest / default) 配下の束縛名を
/// すべて `Unresolvable` として登録する。中身の型は静的に辿れないため owner 推論は
/// しない。収集漏れは shadow 見逃し → import owner への誤帰属 (fail-open) になる
/// ため、未知の構造は全 named children を辿って leaf の identifier を漏らさない。
fn register_pattern_bindings(
    pattern: tree_sitter::Node<'_>,
    source: &[u8],
    bindings: &mut HashMap<String, VarBinding>,
) {
    match pattern.kind() {
        "identifier" | "shorthand_property_identifier_pattern" => {
            if let Ok(name) = pattern.utf8_text(source) {
                register_binding(bindings, name.to_string(), VarBinding::Unresolvable);
            }
        }
        // `{ key: alias }` の key 側は binding ではないため value 側のみ辿る。
        "pair_pattern" => {
            if let Some(v) = pattern.child_by_field_name("value") {
                register_pattern_bindings(v, source, bindings);
            }
        }
        // default 値の右辺 (`{ a = expr }` / `(x = expr)`) は参照であって binding
        // ではないため left のみ辿る。
        "assignment_pattern" | "object_assignment_pattern" => {
            if let Some(l) = pattern.child_by_field_name("left") {
                register_pattern_bindings(l, source, bindings);
            }
        }
        _ => {
            let mut cursor = pattern.walk();
            for child in pattern.named_children(&mut cursor) {
                register_pattern_bindings(child, source, bindings);
            }
        }
    }
}

/// `variable_declarator` / パラメータの型注釈から変数 → owner の束縛を集める。
///
/// owner への静的解決を試みるのは「単純 identifier + 型注釈 / `new` 初期化子」の
/// 経路のみ。それ以外の binding 導入構文 (destructuring / for-of・for-in の宣言付き
/// loop 変数 / catch / bare arrow・JS パラメータ / 関数宣言・named 式の名前) は
/// `Unresolvable` として登録する — 収集しないと receiver 解決が import 由来の
/// class static access へフォールバックし、shadow された名前の票を owner へ誤計上
/// する (dead-code の fail-open)。
fn collect_var_bindings(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    local_to_owner: &HashMap<String, String>,
    owners_set: &HashSet<&str>,
    bindings: &mut HashMap<String, VarBinding>,
) {
    match node.kind() {
        "variable_declarator" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                if name_node.kind() == "identifier" {
                    if let Ok(name) = name_node.utf8_text(source) {
                        // 型注釈が最優先 (`const a: Alpha = ...`)、次に `= new Alpha()`。
                        let owner = node
                            .child_by_field_name("type")
                            .and_then(|t| {
                                type_annotation_owner(t, source, local_to_owner, owners_set)
                            })
                            .or_else(|| {
                                node.child_by_field_name("value")
                                    .and_then(|v| {
                                        new_expression_owner(v, source, local_to_owner, owners_set)
                                    })
                                    .map(VarBinding::Owner)
                            });
                        register_binding(
                            bindings,
                            name.to_string(),
                            owner.unwrap_or(VarBinding::Unresolvable),
                        );
                    }
                } else {
                    // destructuring (`const { a } = ...`) は要素の型を辿れない。
                    register_pattern_bindings(name_node, source, bindings);
                }
            }
        }
        "required_parameter" | "optional_parameter" => {
            if let Some(pat) = node.child_by_field_name("pattern") {
                if pat.kind() == "identifier" {
                    if let Ok(name) = pat.utf8_text(source) {
                        let owner = node.child_by_field_name("type").and_then(|t| {
                            type_annotation_owner(t, source, local_to_owner, owners_set)
                        });
                        register_binding(
                            bindings,
                            name.to_string(),
                            owner.unwrap_or(VarBinding::Unresolvable),
                        );
                    }
                } else {
                    register_pattern_bindings(pat, source, bindings);
                }
            }
        }
        // JS の関数パラメータは formal_parameters 直下に bare pattern で並ぶ
        // (TS は required/optional_parameter に包まれ上の arm が処理する)。
        "formal_parameters" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if !matches!(child.kind(), "required_parameter" | "optional_parameter") {
                    register_pattern_bindings(child, source, bindings);
                }
            }
        }
        // arrow の単独パラメータ (`x => ...`) は formal_parameters に包まれない。
        "arrow_function" => {
            if let Some(p) = node.child_by_field_name("parameter") {
                register_pattern_bindings(p, source, bindings);
            }
        }
        // for-of / for-in の宣言付き loop 変数 (`for (const x of xs)`)。left は
        // lexical_declaration に包まれない bare pattern のため専用処理が要る。
        // kind (var/let/const) の無い `for (x of xs)` は既存変数への代入であって
        // 新規 binding ではないため対象外。
        "for_in_statement" => {
            if node.child_by_field_name("kind").is_some()
                && let Some(left) = node.child_by_field_name("left")
            {
                register_pattern_bindings(left, source, bindings);
            }
        }
        // catch (e) の e。
        "catch_clause" => {
            if let Some(p) = node.child_by_field_name("parameter") {
                register_pattern_bindings(p, source, bindings);
            }
        }
        // 関数宣言 / named function・class 式の名前も値 binding
        // (`function Alpha() {} Alpha.fmt()` は関数オブジェクトへの access)。
        // class_declaration は collect_local_class_definitions が owner 一致時に
        // 正票の源として登録するため対象外。named class **式** の名前は内部スコープ
        // 限定なので、同名の外部 access を owner へ誤帰属しないようこちらで登録する。
        "function_declaration"
        | "generator_function_declaration"
        | "function_expression"
        | "generator_function"
        | "class" => {
            if let Some(n) = node.child_by_field_name("name")
                && n.kind() == "identifier"
                && let Ok(text) = n.utf8_text(source)
            {
                register_binding(bindings, text.to_string(), VarBinding::Unresolvable);
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_var_bindings(child, source, local_to_owner, owners_set, bindings);
    }
}

/// `type_annotation` 配下の型名を owner へ解決する。
/// `Alpha` → `Owner`、`Alpha[]` → `ElementOwner` (subscript access 用)。
fn type_annotation_owner(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    local_to_owner: &HashMap<String, String>,
    owners_set: &HashSet<&str>,
) -> Option<VarBinding> {
    let ty = node.named_child(0)?;
    match ty.kind() {
        "type_identifier" => {
            let text = ty.utf8_text(source).ok()?;
            resolve_class_name(text, local_to_owner, owners_set).map(VarBinding::Owner)
        }
        "array_type" => {
            let elem = ty.named_child(0)?;
            if elem.kind() != "type_identifier" {
                return None;
            }
            let text = elem.utf8_text(source).ok()?;
            resolve_class_name(text, local_to_owner, owners_set).map(VarBinding::ElementOwner)
        }
        _ => None,
    }
}

/// `new Alpha(...)` の constructor 名を owner へ解決する。
fn new_expression_owner(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    local_to_owner: &HashMap<String, String>,
    owners_set: &HashSet<&str>,
) -> Option<String> {
    if node.kind() != "new_expression" {
        return None;
    }
    let ctor = node.child_by_field_name("constructor")?;
    if ctor.kind() != "identifier" {
        return None;
    }
    let text = ctor.utf8_text(source).ok()?;
    resolve_class_name(text, local_to_owner, owners_set)
}

/// クラス名テキストを owner へ解決する。import (alias 含む) または同一ファイル内
/// class 定義に限定する — 単なる owner 名一致での確定は `const Alpha = getBeta();`
/// のような owner と同名の変数を class と誤認するため行わない。
fn resolve_class_name(
    text: &str,
    local_to_owner: &HashMap<String, String>,
    _owners_set: &HashSet<&str>,
) -> Option<String> {
    local_to_owner.get(text).cloned()
}

/// `member_expression` の receiver (object 側) を owner へ静的に解決する。
/// 解決対象: owner 名の static access / `new Owner().m()` / owner 束縛済み変数 /
/// class 内 `this.m()` (enclosing class が owner の場合)。それ以外は `None` (不解決)。
fn resolve_member_receiver(
    member_expr: tree_sitter::Node<'_>,
    source: &[u8],
    local_to_owner: &HashMap<String, String>,
    var_bindings: &HashMap<String, VarBinding>,
    owners_set: &HashSet<&str>,
) -> Option<String> {
    let mut object = member_expr.child_by_field_name("object")?;
    // 冗長括弧 (`(new Owner()).member` / `(alpha).member`) は剥がして実 receiver で
    // 判定する (剥がさないと Ambiguous に落ちて duplicate set の検出力が下がる)。
    while object.kind() == "parenthesized_expression" {
        object = object.named_child(0)?;
    }
    match object.kind() {
        "identifier" => {
            let text = object.utf8_text(source).ok()?;
            // 同名のローカル binding があれば shadow とみなし変数として判定する
            // (`const Alpha = getBeta(); Alpha.fmt()` を class static access と
            // 誤認しない)。binding が無い場合のみ import / 同一ファイル class 名。
            if let Some(binding) = var_bindings.get(text) {
                return match binding {
                    VarBinding::Owner(owner) => Some(owner.clone()),
                    _ => None,
                };
            }
            resolve_class_name(text, local_to_owner, owners_set)
        }
        "new_expression" => new_expression_owner(object, source, local_to_owner, owners_set),
        // `xs[i].member`: xs が owner 配列型 (`Alpha[]`) に束縛されていれば要素は owner。
        "subscript_expression" => {
            let array = object.child_by_field_name("object")?;
            if array.kind() != "identifier" {
                return None;
            }
            let text = array.utf8_text(source).ok()?;
            match var_bindings.get(text) {
                Some(VarBinding::ElementOwner(owner)) => Some(owner.clone()),
                _ => None,
            }
        }
        "this" => {
            let class_name = enclosing_class_name(member_expr, source)?;
            if owners_set.contains(class_name.as_str()) {
                Some(class_name)
            } else {
                // enclosing class が owner でない `this.m()` は継承経由で owner へ
                // 到達し得るため、推測せず不解決に倒す。
                None
            }
        }
        _ => None,
    }
}

/// ノードを囲む class 宣言の名前を返す。
fn enclosing_class_name(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let mut cur = node.parent();
    while let Some(n) = cur {
        if matches!(n.kind(), "class_declaration" | "class") {
            return n
                .child_by_field_name("name")
                .and_then(|name| name.utf8_text(source).ok())
                .map(str::to_string);
        }
        cur = n.parent();
    }
    None
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

#[cfg(test)]
mod tests {
    use super::*;

    fn analyze(src: &str, lang: LangId, owners: &[&str], member: &str) -> FileAnalysis {
        let tree = parser::parse_source(src.as_bytes(), lang).unwrap();
        analyze_file(tree.root_node(), src.as_bytes(), owners, member, lang)
    }

    fn count_for(analysis: &FileAnalysis, owner: &str) -> usize {
        analysis.resolved_counts.get(owner).copied().unwrap_or(0)
    }

    /// 対照: import + 素の static access は owner へ票が入る。
    #[test]
    fn plain_static_access_resolves_to_owner() {
        let a = analyze(
            "import { Alpha } from \"./alpha\";\nAlpha.fmt();\n",
            LangId::Typescript,
            &["Alpha", "Beta"],
            "fmt",
        );
        assert!(!a.has_unresolved_access);
        assert_eq!(count_for(&a, "Alpha"), 1);
    }

    /// for-of の宣言付き loop 変数は owner クラス名を shadow する:
    /// `Alpha.fmt()` を import 由来の static access と誤認して票を入れない。
    #[test]
    fn for_of_loop_variable_shadow_is_unresolved() {
        let a = analyze(
            "import { Alpha } from \"./alpha\";\nimport { Beta } from \"./beta\";\nexport function run(items: Beta[]) {\n  for (const Alpha of items) {\n    Alpha.fmt();\n  }\n}\n",
            LangId::Typescript,
            &["Alpha", "Beta"],
            "fmt",
        );
        assert!(a.has_unresolved_access, "loop 変数 shadow は不解決に倒すべき");
        assert_eq!(count_for(&a, "Alpha"), 0);
    }

    /// kind の無い `for (x of xs)` は代入であって新規 binding ではない:
    /// import owner への static access は引き続き票が入る (過剰 shadow しない)。
    #[test]
    fn for_of_assignment_left_does_not_shadow() {
        let a = analyze(
            "import { Alpha } from \"./alpha\";\nlet x: number;\nexport function run(xs: number[]) {\n  for (x of xs) {\n    Alpha.fmt();\n  }\n}\n",
            LangId::Typescript,
            &["Alpha", "Beta"],
            "fmt",
        );
        assert!(!a.has_unresolved_access);
        assert_eq!(count_for(&a, "Alpha"), 1);
    }

    /// catch パラメータも owner 名を shadow する。
    #[test]
    fn catch_parameter_shadow_is_unresolved() {
        let a = analyze(
            "import { Alpha } from \"./alpha\";\nexport function run() {\n  try {\n    work();\n  } catch (Alpha) {\n    Alpha.fmt();\n  }\n}\n",
            LangId::Typescript,
            &["Alpha", "Beta"],
            "fmt",
        );
        assert!(a.has_unresolved_access, "catch param shadow は不解決に倒すべき");
        assert_eq!(count_for(&a, "Alpha"), 0);
    }

    /// bare arrow パラメータ (`Alpha => ...`) も shadow する。
    #[test]
    fn bare_arrow_parameter_shadow_is_unresolved() {
        let a = analyze(
            "import { Alpha } from \"./alpha\";\nexport const run = (xs: unknown[]) => xs.map(Alpha => Alpha.fmt());\n",
            LangId::Typescript,
            &["Alpha", "Beta"],
            "fmt",
        );
        assert!(a.has_unresolved_access, "bare arrow param shadow は不解決に倒すべき");
        assert_eq!(count_for(&a, "Alpha"), 0);
    }

    /// destructuring binding (`const { Alpha } = ...`) も shadow する。
    #[test]
    fn destructuring_binding_shadow_is_unresolved() {
        let a = analyze(
            "import { Alpha } from \"./alpha\";\nexport function run(bag: { Alpha: unknown }) {\n  const { Alpha } = bag;\n  Alpha.fmt();\n}\n",
            LangId::Typescript,
            &["Alpha", "Beta"],
            "fmt",
        );
        assert!(a.has_unresolved_access, "destructuring shadow は不解決に倒すべき");
        assert_eq!(count_for(&a, "Alpha"), 0);
    }

    /// JS の bare 関数パラメータ (formal_parameters 直下 identifier) も shadow する。
    #[test]
    fn js_bare_function_parameter_shadow_is_unresolved() {
        let a = analyze(
            "import { Alpha } from \"./alpha\";\nexport function run(Alpha) {\n  Alpha.fmt();\n}\n",
            LangId::Javascript,
            &["Alpha", "Beta"],
            "fmt",
        );
        assert!(a.has_unresolved_access, "JS bare param shadow は不解決に倒すべき");
        assert_eq!(count_for(&a, "Alpha"), 0);
    }

    /// 関数宣言名 (`function Alpha() {}`) は関数オブジェクトへの access であり
    /// owner クラスへの票にしない。
    #[test]
    fn function_declaration_name_shadow_is_unresolved() {
        let a = analyze(
            "import { Beta } from \"./beta\";\nfunction Alpha() {}\nAlpha.fmt();\n",
            LangId::Typescript,
            &["Alpha", "Beta"],
            "fmt",
        );
        assert!(a.has_unresolved_access, "関数宣言名 shadow は不解決に倒すべき");
        assert_eq!(count_for(&a, "Alpha"), 0);
    }

    /// 冗長括弧付き receiver (`(new Alpha()).fmt()` / `(alpha).fmt()`) は括弧を
    /// 剥がして実 receiver で解決する (Ambiguous への過剰倒れを防ぐ)。
    #[test]
    fn parenthesized_receiver_is_unwrapped() {
        let a = analyze(
            "import { Alpha } from \"./alpha\";\n(new Alpha()).fmt();\nconst alpha: Alpha = new Alpha();\n(alpha).fmt();\n",
            LangId::Typescript,
            &["Alpha", "Beta"],
            "fmt",
        );
        assert!(!a.has_unresolved_access);
        assert_eq!(count_for(&a, "Alpha"), 2);
    }

    /// 型注釈付きの単純 identifier は従来どおり owner へ解決される (回帰確認)。
    #[test]
    fn typed_variable_still_resolves_to_owner() {
        let a = analyze(
            "import { Alpha } from \"./alpha\";\nexport function run(alpha: Alpha) {\n  alpha.fmt();\n}\n",
            LangId::Typescript,
            &["Alpha", "Beta"],
            "fmt",
        );
        assert!(!a.has_unresolved_access);
        assert_eq!(count_for(&a, "Alpha"), 1);
    }
}
