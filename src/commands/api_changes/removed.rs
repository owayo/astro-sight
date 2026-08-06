//! 削除候補の最終分類。参照 0 件の removed_dead 振り分けと、外部 import / bash / Python 固有の帰属判定を担う。

use super::*;

/// `removed` 候補のうち、HEAD ツリーで repo 内参照 0 件のものを `removed_dead` に
/// 振り分ける。残りは `removed` (破壊的削除) として返す。
/// 参照が 0 件であれば同一 diff 内で全 caller が追随済みと判断し、`api.rm` から除外する。
///
/// 実装上の配慮:
/// - **qualname → bare name**: `Container.method` 形式は refs 検索の identifier
///   マッチでは常に 0 件になるため、`bare_name` で正規化して検索する
/// - **batch refs**: 候補ごとに `find_references` を呼ぶと「候補数 × リポ全体走査」と
///   なる。`find_references_batch` で 1 回の AC + ディレクトリ走査に集約
/// - **同名複数定義の保守扱い**: 削除後の HEAD で同名 def が 2 件以上残っていれば
///   「部分削除」「同名複数 export」など破壊的削除の可能性があるため、保守的に
///   `removed` に残す (false negative より false positive を優先)
/// - **検索失敗時の保守扱い**: batch refs が `Err` を返した場合、すべて `removed`
///   に残す (false negative 防止)
pub(crate) fn partition_removed_dead_candidates(
    dir: &str,
    candidates: Vec<ApiSymbolCandidate>,
) -> (Vec<ApiSymbolCandidate>, Vec<ApiSymbolCandidate>) {
    use crate::models::reference::RefKind;
    use std::collections::{HashMap, HashSet};

    if candidates.is_empty() {
        return (Vec::new(), Vec::new());
    }

    // 候補から bare name を重複排除して集める
    let mut unique_bare: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for c in &candidates {
        let bare = bare_name(&c.name).to_string();
        if seen.insert(bare.clone()) {
            unique_bare.push(bare);
        }
    }

    let service = AppService::new();
    let batch_result = match service.find_references_batch(&unique_bare, dir, None) {
        Ok(r) => r,
        Err(_) => {
            // 検索失敗時は保守的にすべて removed に残す
            return (candidates, Vec::new());
        }
    };

    // 外部パッケージ (package.json deps) から import された同名 binding は、削除した
    // ローカルシンボルとは別物 (例: tailwindcss の `Config` 型) なので参照カウントから
    // 除外する。これがないと汎用名の削除が外部同名 import を拾って api.rm に誤分類される
    // (codex 設計合意。full TS resolver は入れず、証明できる外部 import binding のみ除外)。
    let external_pkgs = load_external_package_names(dir);
    // (path, symbol) -> ファイル解析結果 (外部 import 事実 + 参照帰属の素材)
    let mut facts_cache: HashMap<(String, String), RefAttributionFacts> = HashMap::new();

    // Pass 1: bare_name ごとの残存定義ファイル集合。参照帰属の照合先になる。
    let mut definition_paths: HashMap<String, HashSet<String>> = HashMap::new();
    for r in &batch_result {
        let entry = definition_paths.entry(r.symbol.clone()).or_default();
        for x in &r.references {
            if x.kind == Some(RefKind::Definition) {
                entry.insert(x.path.clone());
            }
        }
    }

    // Pass 2: bare_name -> (def_count, 参照ごとの帰属分類)
    let mut counts: HashMap<String, (usize, Vec<RefAttribution>)> = HashMap::new();
    for r in &batch_result {
        let residual_defs = definition_paths.get(&r.symbol).cloned().unwrap_or_default();
        let mut def_count = 0usize;
        let mut attributions: Vec<RefAttribution> = Vec::new();
        for x in &r.references {
            if x.kind == Some(RefKind::Definition) {
                def_count += 1;
                continue;
            }
            let key = (x.path.clone(), r.symbol.clone());
            let facts = facts_cache.entry(key).or_insert_with(|| {
                analyze_ref_attribution_facts(dir, &x.path, &r.symbol, &external_pkgs)
            });
            // 外部 import specifier の import 元名そのものの参照 (import 行) は別モジュールの
            // export 名なので数えない (`import { Config as X } from "pkg"` の `Config`)。
            if facts.external_source_name_lines.contains(&x.line) {
                continue;
            }
            // 外部 import の local binding を持つファイルなら、その使用箇所も外部由来として
            // 数えない (`import { Config } from "pkg"` の local Config 利用)。
            if facts.external_local_bound {
                continue;
            }
            attributions.push(attribution_for_ref(facts, &x.path, &residual_defs));
        }
        counts.insert(r.symbol.clone(), (def_count, attributions));
    }

    let empty_defs: HashSet<String> = HashSet::new();
    let empty_attrs: (usize, Vec<RefAttribution>) = (0, Vec::new());
    let mut removed_kept = Vec::new();
    let mut removed_dead = Vec::new();
    for c in candidates {
        let bare = bare_name(&c.name).to_string();
        let (def_count, attributions) = counts.get(&bare).unwrap_or(&empty_attrs);
        let residual_defs = definition_paths.get(&bare).unwrap_or(&empty_defs);
        // 全参照が「削除ファイル c.file ではなく残存シンボル由来」と証明できた候補は、
        // 削除シンボルへの残存参照ゼロとして removed_dead (informational) に降格する。
        // 同名定義が複数残る場合 (従来は無条件 kept) も、帰属証明があれば bare name
        // カウントの誤認 (Issue 2026-07-19-bulk-subsystem-removal の shell `usage` /
        // mjs `loadEnvFiles`) を解消できる。証明できない参照が 1 件でもあれば従来どおり
        // removed (blocking) に残す (fail-closed)。
        let candidate_ref = RemovedCandidateRef {
            old_path: &c.file,
            name: &c.name,
            kind: &c.kind,
        };
        if !attributions.is_empty()
            && attributions
                .iter()
                .all(|a| proves_survivor_origin(a, &candidate_ref, residual_defs))
        {
            removed_dead.push(c);
            continue;
        }
        // 同名定義が複数残っている → 保守的に removed に残す
        if *def_count > 1 {
            removed_kept.push(c);
            continue;
        }
        if attributions.is_empty() {
            removed_dead.push(c);
        } else {
            removed_kept.push(c);
        }
    }

    // 第 2 パス: `Owner.member` 形式の removed 候補は、同一 file の型 owner が新ツリーから
    // 完全に消滅している (定義 0 件 かつ 参照 0 件) 場合に限り member も追従して removed_dead
    // へ移す。owner 型がリポジトリ内のどこにも存在しない以上、その owner を import / 生成して
    // member へ到達する静的経路も存在しない。member の bare name カウントだけでは、同一 diff
    // 内で切替先の別クラスが持つ同名メソッド (`listEvents` 等) への参照を「削除メソッドへの
    // 残存参照」と誤認し、owner は informational (rm_dead) なのに member だけ blocking (rm) に
    // 残って hook を止めていた (Issue 2026-07-15-ts-add-refactor-delete-chain-api-rm-fp)。
    //
    // 条件は「owner が removed_dead に居る」だけでは不十分。第 1 パスは def_count <= 1 かつ
    // ref_count == 0 を removed_dead にするため、partial class / open class / extension で
    // 新ツリーに owner の別定義が 1 つ残る (def_count == 1) ケースも removed_dead に入りうる。
    // その場合 owner 型は生存しており member 削除は破壊的変更なので降格してはならない。
    // よって counts が厳密に (0, 0) の owner だけを対象にする。counts に owner が無い
    // (参照検索の欠落) 場合も fail-closed で除外する (codex レビュー指摘)。
    // owner は型 kind (class/struct/trait/interface/enum) に限定し、同名の削除済み関数など
    // による誤降格を防ぐ。
    let dead_type_owner_keys: HashSet<(String, String)> = removed_dead
        .iter()
        .filter(|c| {
            matches!(
                c.kind.as_str(),
                "class" | "struct" | "trait" | "interface" | "enum"
            ) && counts
                .get(bare_name(&c.name))
                .is_some_and(|(def_count, attributions)| *def_count == 0 && attributions.is_empty())
        })
        .map(|c| (c.file.clone(), c.name.clone()))
        .collect();
    if !dead_type_owner_keys.is_empty() {
        let (follow, kept): (Vec<ApiSymbolCandidate>, Vec<ApiSymbolCandidate>) =
            removed_kept.into_iter().partition(|c| {
                matches!(c.kind.as_str(), "method" | "function")
                    && c.name.rsplit_once('.').is_some_and(|(owner, _member)| {
                        dead_type_owner_keys.contains(&(c.file.clone(), owner.to_string()))
                    })
            });
        removed_kept = kept;
        removed_dead.extend(follow);
    }

    (removed_kept, removed_dead)
}

/// `<dir>/package.json` の dependencies / devDependencies / peerDependencies /
/// optionalDependencies のキー (外部パッケージ名) を集める。package.json 不在 / パース
/// 失敗時は空集合 (= 何も除外しない、保守的)。
pub(crate) fn load_external_package_names(dir: &str) -> std::collections::HashSet<String> {
    use std::collections::HashSet;
    let path = std::path::Path::new(dir).join("package.json");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return HashSet::new();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) else {
        return HashSet::new();
    };
    let mut pkgs = HashSet::new();
    for key in [
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
    ] {
        if let Some(obj) = json.get(key).and_then(|v| v.as_object()) {
            for name in obj.keys() {
                pkgs.insert(name.clone());
            }
        }
    }
    pkgs
}

/// import specifier から npm パッケージ名を取り出す。相対 (`./` `../` `/`) / alias
/// (`@/` `~/` `#`) は外部パッケージではないため None (保守的に内部扱い)。scoped は
/// `@scope/pkg`、bare は最初のセグメント。
pub(crate) fn import_specifier_package_name(spec: &str) -> Option<String> {
    if spec.is_empty()
        || spec.starts_with("./")
        || spec.starts_with("../")
        || spec.starts_with('/')
        || spec.starts_with("@/")
        || spec.starts_with("~/")
        || spec.starts_with('#')
    {
        return None;
    }
    if let Some(scoped) = spec.strip_prefix('@') {
        // @scope/pkg[/sub]
        let mut parts = scoped.splitn(3, '/');
        let scope = parts.next()?;
        let pkg = parts.next()?;
        if scope.is_empty() || pkg.is_empty() {
            return None;
        }
        return Some(format!("@{scope}/{pkg}"));
    }
    spec.split('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// 外部パッケージ import 文 `import_stmt` を解析し、`symbol` の local binding 有無を
/// `local_bound` に、import 元名が `symbol` の出現行を `source_name_lines` に記録する。
pub(crate) fn collect_external_import_bindings(
    import_stmt: tree_sitter::Node,
    source: &[u8],
    symbol: &str,
    local_bound: &mut bool,
    source_name_lines: &mut std::collections::HashSet<usize>,
) {
    let mut cursor = import_stmt.walk();
    let Some(clause) = import_stmt
        .named_children(&mut cursor)
        .find(|c| c.kind() == "import_clause")
    else {
        return;
    };
    let mut clause_cursor = clause.walk();
    for child in clause.named_children(&mut clause_cursor) {
        match child.kind() {
            // default import: `import Config from "..."`
            "identifier" => {
                if child.utf8_text(source).ok() == Some(symbol) {
                    *local_bound = true;
                }
            }
            // namespace import: `import * as Config from "..."`
            "namespace_import" => {
                let mut ns = child.walk();
                if child
                    .named_children(&mut ns)
                    .any(|n| n.kind() == "identifier" && n.utf8_text(source).ok() == Some(symbol))
                {
                    *local_bound = true;
                }
            }
            // named imports: `import { Foo, Bar as Baz } from "..."`
            "named_imports" => {
                let mut ni = child.walk();
                for spec in child.named_children(&mut ni) {
                    if spec.kind() != "import_specifier" {
                        continue;
                    }
                    let name_node = spec.child_by_field_name("name");
                    // import 元名が symbol → その出現行を記録 (別モジュールの export 名)
                    if let Some(name) = name_node
                        && name.utf8_text(source).ok() == Some(symbol)
                    {
                        source_name_lines.insert(name.start_position().row);
                    }
                    // local binding (alias があれば alias、無ければ name) が symbol → 利用も外部
                    let local = spec.child_by_field_name("alias").or(name_node);
                    if local.and_then(|n| n.utf8_text(source).ok()) == Some(symbol) {
                        *local_bound = true;
                    }
                }
            }
            _ => {}
        }
    }
}

/// 削除された bash 関数 `name` が、変更後ツリーの bash 系ファイル内のどこからも
/// 参照されていないかを判定する。CLI スクリプトを別言語に書き換えたときに、
/// 新言語側の同名定義/参照を「別物」として扱うため bash ファイル限定で検索する。
/// 参照検索に失敗した場合は保守的に false を返してレビュー対象として残す。
pub(crate) fn is_removed_bash_symbol_unreferenced(dir: &str, name: &str) -> bool {
    let service = AppService::new();
    let Ok(refs_result) = service.find_references(name, dir, None) else {
        return false;
    };
    refs_result
        .references
        .iter()
        .all(|r| !is_bash_script_path(r.path.as_str()))
}

/// 拡張子から bash 系シェルスクリプトファイル（.sh / .bash / .zsh）かを判定する。
pub(crate) fn is_bash_script_path(file_path: &str) -> bool {
    std::path::Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| matches!(ext, "sh" | "bash" | "zsh"))
}

/// `git show <base>:<file_path>` の内容から bash 関数 `name` が `export -f` 等で
/// 明示的にエクスポートされているか判定する。base 側の取得に失敗した場合は
/// 保守的に false（未 export 扱い）を返す。
pub(crate) fn bash_function_is_exported_in_git(
    dir: &str,
    base: &str,
    file_path: &str,
    name: &str,
) -> bool {
    let Some(blob) = git_show_blob(dir, base, file_path) else {
        return false;
    };
    let Ok(text) = std::str::from_utf8(&blob) else {
        return false;
    };
    bash_has_export_f(text, name)
}

/// shell ソース文字列に `export -f <name>` / `declare -fx <name>` / `declare -xf <name>`
/// による関数エクスポート宣言が含まれているかを判定する。
///
/// 各行を `trim_start()` してから先頭一致を見るため、インデント付きの宣言にも対応する。
/// 同一行に複数名を列挙する形式 (`export -f foo bar`) もサポートする。
pub(crate) fn bash_has_export_f(source: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    pub(crate) const PREFIXES: &[&str] = &["export -f ", "declare -fx ", "declare -xf "];
    for line in source.lines() {
        let trimmed = line.trim_start();
        for prefix in PREFIXES {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                for token in rest.split_whitespace() {
                    if token == name {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Python のクラス内に存在するフィールド宣言 (`name: type` 形式) を集める。
///
/// `@property def x(self) -> T` から `@dataclass` フィールド `x: T` への置き換えを検出する
/// ために使う。tree-sitter で `class_definition` を走査し、`name` フィールドが `class_name`
/// と一致するクラスの body 直下にある `name: type` 宣言の左辺 identifier を返す。
pub(crate) fn extract_python_class_fields(
    dir: &str,
    file_path: &str,
    class_name: &str,
) -> std::collections::HashSet<String> {
    let mut fields = std::collections::HashSet::new();
    let full_path = std::path::Path::new(dir).join(file_path);
    let utf8_path = match camino::Utf8Path::from_path(&full_path) {
        Some(p) => p,
        None => return fields,
    };
    let lang_id = match crate::language::LangId::from_path(utf8_path) {
        Ok(l) => l,
        Err(_) => return fields,
    };
    if lang_id != crate::language::LangId::Python {
        return fields;
    }
    let source = match parser::read_file(utf8_path) {
        Ok(s) => s,
        Err(_) => return fields,
    };
    let tree = match parser::parse_source(&source, lang_id) {
        Ok(t) => t,
        Err(_) => return fields,
    };

    walk_python_class_for_fields(tree.root_node(), &source, class_name, &mut fields);
    fields
}

pub(crate) fn walk_python_class_for_fields(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    class_name: &str,
    out: &mut std::collections::HashSet<String>,
) {
    if node.kind() == "class_definition"
        && let Some(name_node) = node.child_by_field_name("name")
        && name_node.utf8_text(source).ok() == Some(class_name)
        && let Some(body) = node.child_by_field_name("body")
    {
        collect_python_dataclass_fields(body, source, out);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_python_class_for_fields(child, source, class_name, out);
    }
}

/// Python のクラス body 直下にある `name: type` 形式の宣言の左辺 identifier を集める。
///
/// tree-sitter-python では `name: type` (右辺なし) は `expression_statement > assignment`
/// に展開され、`assignment.left = identifier` / `assignment.type` が存在する。`name: type = default`
/// の形式も同じく `assignment` ノードで `right` が追加されるだけなので同じハンドラで取れる。
pub(crate) fn collect_python_dataclass_fields(
    body: tree_sitter::Node<'_>,
    source: &[u8],
    out: &mut std::collections::HashSet<String>,
) {
    let mut cursor = body.walk();
    for stmt in body.children(&mut cursor) {
        if stmt.kind() != "expression_statement" {
            continue;
        }
        let mut sub_cursor = stmt.walk();
        for sub in stmt.children(&mut sub_cursor) {
            if sub.kind() != "assignment" {
                continue;
            }
            let Some(left) = sub.child_by_field_name("left") else {
                continue;
            };
            if left.kind() != "identifier" {
                continue;
            }
            // `type` フィールドが存在するもの（typed annotation）のみ対象
            if sub.child_by_field_name("type").is_none() {
                continue;
            }
            if let Ok(name) = left.utf8_text(source) {
                out.insert(name.to_string());
            }
        }
    }
}

/// Python の `@property def member(self) -> T` を `@dataclass` フィールド `member: T` に
/// 置き換えた変更を検出する。
///
/// `qualname` は `Container.member` 形式の文字列。`diff_new_paths` 内のいずれかの新ファイルに
/// 同名 `Container` クラスが存在し、その中に `member: type` の typed annotation 宣言が
/// あれば、それが置き換え先のファイルパスであるとして返す。複数候補があれば最初のものを返す。
///
/// `old_path` は削除シンボルの元ファイル。Python 以外なら対象外 (他言語の `Container.member`
/// 削除が、diff 内 .py の偶然の同名 class+field で informational に降格するのを防ぐ)。
pub(crate) fn detect_python_property_to_field(
    dir: &str,
    old_path: &str,
    qualname: &str,
    diff_new_paths: &HashSet<String>,
) -> Option<String> {
    if !matches!(
        crate::language::LangId::from_path(camino::Utf8Path::new(old_path)),
        Ok(crate::language::LangId::Python)
    ) {
        return None;
    }
    let (container, member) = qualname.split_once('.')?;
    if container.is_empty() || member.is_empty() {
        return None;
    }
    // qualname がさらにネストしている場合 (`A.B.member`) は保守的に対象外とする。
    if member.contains('.') {
        return None;
    }
    // 複数の新規ファイルが同名クラス・同名フィールドを持つ場合に「最初の 1 件」を
    // 決めるため、パス昇順で走査する。`diff_new_paths` は HashSet なので、そのまま
    // 反復すると実行ごとに違うファイルが報告先になる。
    let mut candidates: Vec<&String> = diff_new_paths.iter().collect();
    candidates.sort_unstable();
    for new_path in candidates {
        if !std::path::Path::new(new_path)
            .extension()
            .and_then(|s| s.to_str())
            .map(|ext| ext.eq_ignore_ascii_case("py"))
            .unwrap_or(false)
        {
            continue;
        }
        let fields = extract_python_class_fields(dir, new_path, container);
        if fields.contains(member) {
            return Some(new_path.clone());
        }
    }
    None
}
