//! api.rm 候補の参照帰属解決 (Issue 2026-07-19-bulk-subsystem-removal)。
//!
//! `partition_removed_dead_candidates` の bare name 参照カウントは、削除シンボルと同名の
//! 残存シンボル (別ファイルの同名 shell 関数や同名 export) への参照を「削除シンボルへの
//! 残存参照」と誤認し、意図的な bulk removal を blocking (api.rm) に残す。参照 1 件ごとに
//! 「残存シンボル由来である」ことの証明を試み、全参照が証明できた候補だけを removed_dead
//! (informational) へ降格する。証明できない参照は従来どおり残存参照として数える
//! (fail-closed、破壊的削除の見逃しを作らない)。

use std::collections::HashSet;
use std::rc::Rc;

use crate::engine::parser;
use crate::language::LangId;

/// 参照 1 件の帰属解決結果。candidate (削除元 old_path) 非依存の事実を保持し、
/// `proves_survivor_origin` で old_path / 残存定義ファイル集合と照合する。
#[derive(Clone)]
pub(crate) struct RefAttribution {
    /// 参照ファイルの言語。解決できない場合は None (言語による証明はしない = fail-closed)。
    pub(crate) ref_lang: Option<LangId>,
    pub(crate) origin: RefOrigin,
}

/// 参照が「残存シンボル由来」であることの根拠。
#[derive(Clone)]
pub(crate) enum RefOrigin {
    /// 参照ファイル自身に同名定義が残存している (TS/JS/bash)。TS/JS は削除ファイルからの
    /// 同名 import が同ファイル定義と共存できない (duplicate declaration) ため、bash は
    /// 削除ファイルが消えても同ファイル定義が残り未定義呼び出しにならないため、削除
    /// シンボル由来ではないと言える。bash はリテラル `source` が削除ファイルを指す場合
    /// のみ証明失敗 (削除実装への明示依存が残っている)。
    SelfDefined {
        /// bash のみ Some: リテラル source の解決候補 (repo 相対)。
        sourced_candidates: Option<Rc<HashSet<String>>>,
    },
    /// TS/JS: symbol がこのファイルの from 句付き import / re-export 文の識別子として
    /// 出現し、その specifier の解決候補が `candidates`。残存定義ファイルへ解決できれば
    /// 「残存シンボルへの束縛」と証明できる。
    ImportResolved { candidates: Rc<HashSet<String>> },
    /// Python: symbol の出現がすべて属性アクセス (`re.search` / `self.search`) で、
    /// レシーバの実効モジュール名が `receiver_module_names`。モジュールレベルの自由関数 /
    /// クラスは `<module>.<name>` 形式でしか属性アクセス経由に到達できないため、
    /// レシーバ名の中に削除モジュール名が無ければ残存シンボル由来と証明できる。
    PythonAttributeAccess {
        receiver_module_names: Rc<HashSet<String>>,
    },
    /// 証明不能。従来どおり残存参照として数える。
    Unproven,
}

/// `proves_survivor_origin` の照合対象となる削除候補。同型 `&str` の `old_path` / `name` /
/// `kind` を位置引数で渡すと取り違えてもコンパイルが通るため構造体で束ねる。
pub(crate) struct RemovedCandidateRef<'a> {
    /// 削除元ファイル (repo 相対)。
    pub(crate) old_path: &'a str,
    /// 削除シンボル名。qualname (`BM25.fit`) はメソッドを表す。
    pub(crate) name: &'a str,
    /// 削除シンボルの kind (`function` / `method` / `class` ...)。
    pub(crate) kind: &'a str,
}

/// `attr` が「削除ファイル `candidate.old_path` のシンボルではなく残存シンボル由来」と
/// 証明できるか。`residual_def_paths` は同 bare name の残存定義ファイル集合 (repo 相対)。
pub(crate) fn proves_survivor_origin(
    attr: &RefAttribution,
    candidate: &RemovedCandidateRef<'_>,
    residual_def_paths: &HashSet<String>,
) -> bool {
    // 参照ファイルの言語から削除ファイルの言語へ識別子束縛の経路が無いなら、同名でも
    // 別物と断言できる。polyglot リポジトリでは汎用名 (`search` / `init`) の bare name
    // 参照の大半がこれに当たる (Issue 2026-08-06-api-rm-atomic-module-deletion では
    // 2,522 件中 2,521 件が Python 削除に対する PHP / C / JS / TS の同名参照だった)。
    if let (Some(ref_lang), Some(old_lang)) = (attr.ref_lang, path_lang(candidate.old_path))
        && !ref_lang.can_reference_definition_in(old_lang)
    {
        return true;
    }
    match &attr.origin {
        RefOrigin::SelfDefined { sourced_candidates } => sourced_candidates
            .as_ref()
            .is_none_or(|sourced| !sourced.contains(candidate.old_path)),
        RefOrigin::ImportResolved { candidates } => {
            !candidates.contains(candidate.old_path)
                && candidates.iter().any(|c| residual_def_paths.contains(c))
        }
        RefOrigin::PythonAttributeAccess {
            receiver_module_names,
        } => {
            // メソッド削除 (`BM25.fit`) では属性アクセスこそが正当な参照形なので証明しない。
            // qualname でなくとも kind が method なら同様に fail-closed に倒す。
            if candidate.kind == "method" || candidate.name.contains('.') {
                return false;
            }
            let Some(module) = python_module_name(candidate.old_path) else {
                return false;
            };
            !receiver_module_names.contains(&module)
        }
        RefOrigin::Unproven => false,
    }
}

/// 削除元 Python ファイルのモジュール名。`pkg/core.py` → `core`、
/// `pkg/__init__.py` → `pkg` (パッケージ名で参照されるため)。
fn python_module_name(old_path: &str) -> Option<String> {
    let path = std::path::Path::new(old_path);
    let stem = path.file_stem()?.to_str()?;
    if stem == "__init__" {
        return path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(str::to_string);
    }
    Some(stem.to_string())
}

/// (ref_path, symbol) 単位のファイル解析結果。参照ループでキャッシュされ、
/// `attribution_for_ref` で `RefAttribution` に変換される。
pub(crate) struct RefAttributionFacts {
    /// 外部パッケージ import の local binding が symbol
    /// (従来の analyze_external_import_for_symbol と同義)。
    pub external_local_bound: bool,
    /// 外部 import 元名が symbol の行集合 (0-indexed、従来互換)。
    pub external_source_name_lines: HashSet<usize>,
    /// symbol が相対 import / re-export の識別子として出現する文の解決候補 (和集合)。
    /// None = from 句付き文に symbol の出現なし。
    pub local_import_candidates: Option<Rc<HashSet<String>>>,
    /// symbol が相対解決できない from 句 (alias / workspace パッケージ / 動的文字列) の
    /// import / re-export 識別子として出現する。fail-closed で全証明を封じる。
    pub has_unresolvable_import_binding: bool,
    /// bash のみ Some: リテラル source の解決候補。
    pub bash_sourced_candidates: Option<Rc<HashSet<String>>>,
    /// Python のみ Some: symbol の非定義出現がすべて属性アクセスだった場合の、
    /// レシーバの実効モジュール名集合。1 件でも bare identifier 出現があれば None。
    pub python_attribute_receivers: Option<Rc<HashSet<String>>>,
    /// 参照ファイルの言語。解決できなければ None。
    pub ref_lang: Option<LangId>,
    /// ファイルが TS/JS 系か。
    pub is_js_ts: bool,
    /// ファイルが bash 系か。
    pub is_bash: bool,
}

impl RefAttributionFacts {
    fn opaque() -> Self {
        Self {
            external_local_bound: false,
            external_source_name_lines: HashSet::new(),
            local_import_candidates: None,
            has_unresolvable_import_binding: false,
            bash_sourced_candidates: None,
            python_attribute_receivers: None,
            ref_lang: None,
            is_js_ts: false,
            is_bash: false,
        }
    }
}

/// facts と残存定義ファイル集合から参照 1 件の帰属を分類する。
/// `ref_path` は参照が見つかったファイル (repo 相対)。
pub(crate) fn attribution_for_ref(
    facts: &RefAttributionFacts,
    ref_path: &str,
    residual_def_paths: &HashSet<String>,
) -> RefAttribution {
    RefAttribution {
        ref_lang: facts.ref_lang,
        origin: ref_origin_for(facts, ref_path, residual_def_paths),
    }
}

fn ref_origin_for(
    facts: &RefAttributionFacts,
    ref_path: &str,
    residual_def_paths: &HashSet<String>,
) -> RefOrigin {
    if facts.is_js_ts {
        if facts.has_unresolvable_import_binding {
            return RefOrigin::Unproven;
        }
        // from 句付き文で束縛 / 再輸出されている場合は同ファイル定義より優先して
        // import 解決で判定する (`export { x } from './deleted'` は同ファイル定義と
        // 共存でき、削除で壊れるため SelfDefined で証明してはならない)。
        if let Some(candidates) = &facts.local_import_candidates {
            return RefOrigin::ImportResolved {
                candidates: Rc::clone(candidates),
            };
        }
        if residual_def_paths.contains(ref_path) {
            return RefOrigin::SelfDefined {
                sourced_candidates: None,
            };
        }
        return RefOrigin::Unproven;
    }
    if facts.is_bash && residual_def_paths.contains(ref_path) {
        return RefOrigin::SelfDefined {
            sourced_candidates: facts.bash_sourced_candidates.clone(),
        };
    }
    if let Some(receivers) = &facts.python_attribute_receivers {
        return RefOrigin::PythonAttributeAccess {
            receiver_module_names: Rc::clone(receivers),
        };
    }
    RefOrigin::Unproven
}

/// `ref_path` のファイルを 1 回だけ parse し、外部 import 事実 (従来) と参照帰属の素材を
/// まとめて返す。非対応言語 / 読み込み・parse 失敗は「何も証明できない」facts を返す
/// (従来どおりカウントされる、保守的)。
pub(crate) fn analyze_ref_attribution_facts(
    dir: &str,
    ref_path: &str,
    symbol: &str,
    external_pkgs: &HashSet<String>,
) -> RefAttributionFacts {
    let abs = if std::path::Path::new(ref_path).is_absolute() {
        std::path::PathBuf::from(ref_path)
    } else {
        std::path::Path::new(dir).join(ref_path)
    };
    let Some(utf8) = camino::Utf8Path::from_path(&abs) else {
        return RefAttributionFacts::opaque();
    };
    let Some(lang) = file_lang(utf8) else {
        return RefAttributionFacts::opaque();
    };
    let mut facts = match lang {
        LangId::Javascript | LangId::Typescript | LangId::Tsx => {
            analyze_js_ts_facts(utf8, lang, ref_path, symbol, external_pkgs)
        }
        LangId::Bash => analyze_bash_facts(utf8, ref_path),
        LangId::Python => analyze_python_facts(utf8, symbol),
        _ => RefAttributionFacts::opaque(),
    };
    facts.ref_lang = Some(lang);
    facts
}

/// 参照ファイルの言語。拡張子で決まらない場合は shebang まで見る
/// (`scripts/install_prereq` のような拡張子なしシェルスクリプトも参照元になり得る)。
fn file_lang(utf8: &camino::Utf8Path) -> Option<LangId> {
    if let Ok(lang) = LangId::from_path(utf8) {
        return Some(lang);
    }
    // Angular テンプレート。astro-sight が `.html` から参照を出すのは component template
    // 走査経路だけで、その識別子は TS component クラスのメンバに解決される。
    if matches!(
        utf8.extension().map(str::to_ascii_lowercase).as_deref(),
        Some("html") | Some("htm")
    ) {
        return Some(LangId::Typescript);
    }
    let source = parser::read_file(utf8).ok()?;
    let first_line = source.split(|b| *b == b'\n').next()?;
    LangId::from_shebang(std::str::from_utf8(first_line).ok()?)
}

/// 削除元ファイルの言語。削除済みで実体を読めないため拡張子だけで判定する。
fn path_lang(path: &str) -> Option<LangId> {
    LangId::from_path(camino::Utf8Path::new(path)).ok()
}

/// Python の参照ファイルを解析し、symbol の非定義出現がすべて属性アクセス
/// (`re.search` / `self.search` / `pkg.mod.search`) かどうかを判定する。
///
/// すべて属性アクセスなら、レシーバの「実効モジュール名」集合を返す。実効モジュール名は
/// レシーバ式の末尾識別子を、そのファイルの `as` 別名 (`import pkg.core as c` / `from pkg
/// import core as c`) で解決したもの。モジュールレベルの自由関数・クラスへ属性アクセスで
/// 到達する経路は `<module>.<name>` だけなので、この集合に削除モジュール名が無ければ
/// 削除シンボル由来ではないと言える。
///
/// bare identifier としての出現が 1 件でもあれば None を返し従来どおり残存参照として数える
/// (`from core import search` の import 行や `search()` の直接呼び出しがこれに当たる)。
fn analyze_python_facts(utf8: &camino::Utf8Path, symbol: &str) -> RefAttributionFacts {
    let Ok(source) = parser::read_file(utf8) else {
        return RefAttributionFacts::opaque();
    };
    let Ok(tree) = parser::parse_source(&source, LangId::Python) else {
        return RefAttributionFacts::opaque();
    };
    let root = tree.root_node();
    let aliases = collect_python_import_aliases(root, &source);
    let mut receivers: HashSet<String> = HashSet::new();
    if !collect_python_attribute_receivers(root, &source, symbol, &aliases, &mut receivers) {
        return RefAttributionFacts::opaque();
    }
    RefAttributionFacts {
        python_attribute_receivers: Some(Rc::new(receivers)),
        ..RefAttributionFacts::opaque()
    }
}

/// `import pkg.core as c` / `from pkg import core as c` の別名 → 実モジュール名 (末尾segment)。
/// 別名を持たない import は識別子がそのままモジュール名になるため収集不要。
fn collect_python_import_aliases(
    node: tree_sitter::Node,
    source: &[u8],
) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    fn walk(
        node: tree_sitter::Node,
        source: &[u8],
        out: &mut std::collections::HashMap<String, String>,
    ) {
        if node.kind() == "aliased_import"
            && let Some(name) = node.child_by_field_name("name")
            && let Some(alias) = node.child_by_field_name("alias")
            && let Ok(alias_text) = alias.utf8_text(source)
            && let Ok(name_text) = name.utf8_text(source)
        {
            let module = name_text.rsplit('.').next().unwrap_or(name_text);
            out.insert(alias_text.to_string(), module.to_string());
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            walk(child, source, out);
        }
    }
    walk(node, source, &mut out);
    out
}

/// symbol の出現を走査し、すべてが属性アクセスなら true を返してレシーバの実効モジュール名を
/// `out` に積む。bare identifier としての出現を 1 件でも見つけたら false (証明不能)。
fn collect_python_attribute_receivers(
    node: tree_sitter::Node,
    source: &[u8],
    symbol: &str,
    aliases: &std::collections::HashMap<String, String>,
    out: &mut HashSet<String>,
) -> bool {
    if matches!(node.kind(), "string" | "comment") {
        return true;
    }
    if node.kind() == "identifier" && node.utf8_text(source).ok() == Some(symbol) {
        let Some(parent) = node.parent() else {
            return false;
        };
        // `OBJ.symbol` の attribute 位置のみ属性アクセスとして扱う。
        // `symbol.attr` の object 位置は bare identifier 参照。
        if parent.kind() != "attribute"
            || parent.child_by_field_name("attribute").map(|n| n.id()) != Some(node.id())
        {
            return false;
        }
        let Some(object) = parent.child_by_field_name("object") else {
            return false;
        };
        if let Some(name) = python_receiver_effective_name(object, source, aliases) {
            out.insert(name);
        } else {
            // レシーバの末尾識別子が取れない (添字・呼び出し結果など)。モジュール参照では
            // ないため証明を妨げないが、判定材料にもしない。
        }
        return true;
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .all(|child| collect_python_attribute_receivers(child, source, symbol, aliases, out))
}

/// レシーバ式の実効モジュール名。`re` → `re`、`pkg.core` → `core`、`c` (別名) → `core`。
/// 添字・関数呼び出しなど識別子で終わらない式は None。
fn python_receiver_effective_name(
    object: tree_sitter::Node,
    source: &[u8],
    aliases: &std::collections::HashMap<String, String>,
) -> Option<String> {
    let last = match object.kind() {
        "identifier" => object,
        "attribute" => object.child_by_field_name("attribute")?,
        _ => return None,
    };
    let text = last.utf8_text(source).ok()?;
    Some(
        aliases
            .get(text)
            .cloned()
            .unwrap_or_else(|| text.to_string()),
    )
}

fn analyze_js_ts_facts(
    utf8: &camino::Utf8Path,
    lang: LangId,
    ref_path: &str,
    symbol: &str,
    external_pkgs: &HashSet<String>,
) -> RefAttributionFacts {
    let Ok(source) = parser::read_file(utf8) else {
        return RefAttributionFacts::opaque();
    };
    let Ok(tree) = parser::parse_source(&source, lang) else {
        return RefAttributionFacts::opaque();
    };
    let root = tree.root_node();
    let mut facts = RefAttributionFacts {
        is_js_ts: true,
        ..RefAttributionFacts::opaque()
    };
    let mut local_candidates: HashSet<String> = HashSet::new();
    let mut has_local_binding = false;
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        let kind = child.kind();
        if kind != "import_statement" && kind != "export_statement" {
            continue;
        }
        let Some(src_node) = child.child_by_field_name("source") else {
            continue; // from 句なし export はローカル export なので対象外
        };
        let Some(spec) = super::static_js_string_text(src_node, &source) else {
            // 文字列が静的に取れない from 句に symbol が出現していれば fail-closed。
            if statement_mentions_symbol(child, &source, symbol) {
                facts.has_unresolvable_import_binding = true;
            }
            continue;
        };
        if let Some(pkg) = super::import_specifier_package_name(spec) {
            // 外部 import: 従来の external 除外情報を収集する。external_pkgs に無い
            // bare specifier (workspace パッケージ等) は解決不能扱い。
            if external_pkgs.contains(&pkg) {
                if kind == "import_statement" {
                    super::collect_external_import_bindings(
                        child,
                        &source,
                        symbol,
                        &mut facts.external_local_bound,
                        &mut facts.external_source_name_lines,
                    );
                }
                // 外部パッケージへの re-export (`export { x } from "pkg"`) に symbol が
                // 出現しても削除ローカルシンボルとは別物なので束縛扱いしない。
                continue;
            }
            if statement_mentions_symbol(child, &source, symbol) {
                facts.has_unresolvable_import_binding = true;
            }
            continue;
        }
        // 相対 specifier / alias。symbol が出現する文だけ解決を試みる。
        if !statement_mentions_symbol(child, &source, symbol) {
            continue;
        }
        match relative_import_candidates(ref_path, spec) {
            Some(candidates) => {
                has_local_binding = true;
                local_candidates.extend(candidates);
            }
            None => {
                facts.has_unresolvable_import_binding = true;
            }
        }
    }
    if has_local_binding {
        facts.local_import_candidates = Some(Rc::new(local_candidates));
    }
    facts
}

/// import / export 文のノード配下 (source 文字列を除く) に識別子 `symbol` が出現するか。
/// named import / export の name・alias、default import、namespace import をまとめて
/// 拾う (どの位置であれ、出現があればその文の from 先が参照の由来になり得る)。
fn statement_mentions_symbol(stmt: tree_sitter::Node, source: &[u8], symbol: &str) -> bool {
    fn walk(node: tree_sitter::Node, source: &[u8], symbol: &str) -> bool {
        if matches!(node.kind(), "string" | "string_fragment") {
            return false;
        }
        if matches!(node.kind(), "identifier" | "property_identifier")
            && node.utf8_text(source).ok() == Some(symbol)
        {
            return true;
        }
        let mut cursor = node.walk();
        node.named_children(&mut cursor)
            .any(|c| walk(c, source, symbol))
    }
    walk(stmt, source, symbol)
}

fn analyze_bash_facts(utf8: &camino::Utf8Path, ref_path: &str) -> RefAttributionFacts {
    let Ok(source) = parser::read_file(utf8) else {
        return RefAttributionFacts::opaque();
    };
    let Ok(tree) = parser::parse_source(&source, LangId::Bash) else {
        return RefAttributionFacts::opaque();
    };
    let mut sourced = HashSet::new();
    collect_bash_literal_sources(tree.root_node(), &source, ref_path, &mut sourced);
    RefAttributionFacts {
        is_bash: true,
        bash_sourced_candidates: Some(Rc::new(sourced)),
        ..RefAttributionFacts::opaque()
    }
}

/// bash AST から `source <path>` / `. <path>` のリテラル引数を集め、repo 相対の解決候補に
/// 展開する。変数・コマンド置換を含む引数は「削除ファイルを指す証拠」にならないため
/// 候補に入れない (このガードは SelfDefined 証明を blocking に戻す用途のみで、候補が
/// 増える方向は false negative を生まない)。
fn collect_bash_literal_sources(
    node: tree_sitter::Node,
    source: &[u8],
    ref_path: &str,
    out: &mut HashSet<String>,
) {
    if node.kind() == "command" {
        let mut cursor = node.walk();
        let children: Vec<_> = node.named_children(&mut cursor).collect();
        if let Some(name) = children.first().filter(|c| c.kind() == "command_name")
            && matches!(name.utf8_text(source).ok(), Some("source") | Some("."))
            && let Some(arg) = children.get(1)
            && let Some(literal) = bash_literal_word(*arg, source)
        {
            add_bash_source_candidates(ref_path, &literal, out);
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_bash_literal_sources(child, source, ref_path, out);
    }
}

/// bash の引数ノードが expansion を含まないリテラルであればテキストを返す。
fn bash_literal_word(node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    match node.kind() {
        "word" => node.utf8_text(source).ok().map(str::to_string),
        "string" => {
            // `"lib.sh"`: named child が string_content のみならリテラル
            let mut cursor = node.walk();
            let children: Vec<_> = node.named_children(&mut cursor).collect();
            if children.len() == 1 && children[0].kind() == "string_content" {
                children[0].utf8_text(source).ok().map(str::to_string)
            } else {
                None
            }
        }
        "raw_string" => node
            .utf8_text(source)
            .ok()
            .map(|t| t.trim_matches('\'').to_string()),
        _ => None,
    }
}

/// source 引数の解決候補。bash の source は実行時 cwd 依存のため、参照ファイルの親
/// ディレクトリ相対と repo ルート相対の両方を候補にする (候補が増える方向は blocking
/// 維持側にしか働かない)。絶対パスは repo 相対比較できないため対象外。
fn add_bash_source_candidates(ref_path: &str, arg: &str, out: &mut HashSet<String>) {
    if arg.starts_with('/') || arg.is_empty() {
        return;
    }
    let rel_spec;
    let spec = if arg.starts_with("./") || arg.starts_with("../") {
        arg
    } else {
        rel_spec = format!("./{arg}");
        &rel_spec
    };
    if let Some(p) = normalize_relative_specifier(ref_path, spec) {
        out.insert(p);
    }
    // repo ルート相対 (cwd = repo root で実行される慣習)
    if let Some(p) = normalize_repo_root_relative(arg) {
        out.insert(p);
    }
}

/// repo 相対パス `base_file` の親ディレクトリを基準に、相対 specifier `spec`
/// (`./x` / `../x`) を repo 相対へ正規化する。`..` がリポジトリルートを突き抜ける場合や
/// 相対 specifier でない場合は None。
pub(crate) fn normalize_relative_specifier(base_file: &str, spec: &str) -> Option<String> {
    if !(spec.starts_with("./") || spec.starts_with("../")) {
        return None;
    }
    let mut components: Vec<&str> = base_file.split('/').collect();
    components.pop(); // ファイル名を除き親 dir に
    for seg in spec.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                components.pop()?;
            }
            other => components.push(other),
        }
    }
    if components.is_empty() {
        return None;
    }
    Some(components.join("/"))
}

/// `arg` (相対形式) を repo ルート基準で正規化する。`..` 成分がルートを突き抜ける場合は None。
fn normalize_repo_root_relative(arg: &str) -> Option<String> {
    let mut components: Vec<&str> = Vec::new();
    for seg in arg.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                components.pop()?;
            }
            other => components.push(other),
        }
    }
    if components.is_empty() {
        return None;
    }
    Some(components.join("/"))
}

/// TS/JS の相対 import specifier を実体ファイルの解決候補 (repo 相対パス集合) に展開する。
/// - `.js` / `.mjs` / `.cjs` 指定は TS 実体 (`.ts` / `.tsx` / `.mts` / `.cts`) も候補に含める
///   (moduleResolution=node16/bundler の ESM 慣習)
/// - JS 系拡張子なしは各拡張子の付与と `index.*` を展開する
///
/// 相対でない / ルート突き抜けは None (解決不能)。
pub(crate) fn relative_import_candidates(base_file: &str, spec: &str) -> Option<HashSet<String>> {
    const EXTS: [&str; 6] = ["ts", "tsx", "js", "jsx", "mjs", "cjs"];
    let normalized = normalize_relative_specifier(base_file, spec)?;
    let mut out = HashSet::new();
    let known_ext = std::path::Path::new(&normalized)
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| EXTS.contains(e) || matches!(*e, "mts" | "cts"))
        .map(str::to_string);
    match known_ext.as_deref() {
        Some("js") | Some("jsx") => {
            let stem = normalized.rsplit_once('.').map(|(s, _)| s).unwrap_or("");
            out.insert(format!("{stem}.ts"));
            out.insert(format!("{stem}.tsx"));
            out.insert(normalized);
        }
        Some("mjs") => {
            let stem = normalized.rsplit_once('.').map(|(s, _)| s).unwrap_or("");
            out.insert(format!("{stem}.mts"));
            out.insert(normalized);
        }
        Some("cjs") => {
            let stem = normalized.rsplit_once('.').map(|(s, _)| s).unwrap_or("");
            out.insert(format!("{stem}.cts"));
            out.insert(normalized);
        }
        Some(_) => {
            out.insert(normalized);
        }
        None => {
            for e in EXTS {
                out.insert(format!("{normalized}.{e}"));
            }
            for e in EXTS {
                out.insert(format!("{normalized}/index.{e}"));
            }
            out.insert(normalized);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_relative_specifier_resolves_parent_dirs() {
        assert_eq!(
            normalize_relative_specifier("api/test/config.test.ts", "../src/config").as_deref(),
            Some("api/src/config")
        );
        assert_eq!(
            normalize_relative_specifier("src/index.ts", "./services/gws").as_deref(),
            Some("src/services/gws")
        );
        // ルート突き抜けは None
        assert_eq!(normalize_relative_specifier("a.ts", "../../x"), None);
        // 相対でない specifier は None
        assert_eq!(normalize_relative_specifier("a.ts", "pkg"), None);
        assert_eq!(normalize_relative_specifier("a.ts", "@/lib"), None);
    }

    #[test]
    fn relative_import_candidates_expands_extensions_and_index() {
        let c = relative_import_candidates("api/test/config.test.ts", "../src/config")
            .expect("resolved");
        assert!(c.contains("api/src/config.ts"));
        assert!(c.contains("api/src/config.mjs"));
        assert!(c.contains("api/src/config/index.ts"));
        // `.js` 指定は TS 実体も候補に
        let c = relative_import_candidates("src/a.ts", "./b.js").expect("resolved");
        assert!(c.contains("src/b.js"));
        assert!(c.contains("src/b.ts"));
        assert!(c.contains("src/b.tsx"));
        // 拡張子付きピリオド入りファイル名 (`.util` は JS 拡張子でない) は拡張子なし扱い
        let c = relative_import_candidates("src/a.ts", "./b.util").expect("resolved");
        assert!(c.contains("src/b.util.ts"));
    }

    /// 言語による証明を挟まない (= 従来ロジックのみを見る) ための helper。
    /// `ref_lang` を None にすると `proves_survivor_origin` の言語判定を必ず素通りする。
    fn attr(origin: RefOrigin) -> RefAttribution {
        RefAttribution {
            ref_lang: None,
            origin,
        }
    }

    fn candidate<'a>(old_path: &'a str, name: &'a str, kind: &'a str) -> RemovedCandidateRef<'a> {
        RemovedCandidateRef {
            old_path,
            name,
            kind,
        }
    }

    #[test]
    fn proves_survivor_origin_matrix() {
        let defs: HashSet<String> = ["api/src/config.ts".to_string()].into();
        let deleted = candidate("plugins/setup.mjs", "helper", "function");
        // 残存定義への import 解決 → 証明
        let a = attr(RefOrigin::ImportResolved {
            candidates: Rc::new(
                [
                    "api/src/config.ts".to_string(),
                    "api/src/config.js".to_string(),
                ]
                .into(),
            ),
        });
        assert!(proves_survivor_origin(&a, &deleted, &defs));
        // 候補に old_path が含まれる → 証明失敗 (削除ファイルへの残存参照)
        let a = attr(RefOrigin::ImportResolved {
            candidates: Rc::new(["plugins/setup.mjs".to_string()].into()),
        });
        assert!(!proves_survivor_origin(&a, &deleted, &defs));
        // 候補が残存定義と交差しない (re-export barrel 等) → 証明失敗
        let a = attr(RefOrigin::ImportResolved {
            candidates: Rc::new(["lib/barrel.ts".to_string()].into()),
        });
        assert!(!proves_survivor_origin(&a, &deleted, &defs));
        // 同ファイル定義 (TS/JS) → 証明
        let a = attr(RefOrigin::SelfDefined {
            sourced_candidates: None,
        });
        assert!(proves_survivor_origin(&a, &deleted, &defs));
        // bash: リテラル source が old_path を指す → 証明失敗
        let a = attr(RefOrigin::SelfDefined {
            sourced_candidates: Some(Rc::new(["scripts/deleted.sh".to_string()].into())),
        });
        assert!(!proves_survivor_origin(
            &a,
            &candidate("scripts/deleted.sh", "helper", "function"),
            &defs
        ));
        assert!(proves_survivor_origin(
            &a,
            &candidate("scripts/other.sh", "helper", "function"),
            &defs
        ));
        // 証明不能は常に false
        assert!(!proves_survivor_origin(
            &attr(RefOrigin::Unproven),
            &candidate("x.ts", "helper", "function"),
            &defs
        ));
    }

    /// 言語が非互換なら origin が Unproven でも証明が成立する。互換なら従来どおり従う。
    #[test]
    fn proves_survivor_origin_uses_cross_language_incompatibility() {
        let defs: HashSet<String> = HashSet::new();
        let deleted = candidate("scripts/core.py", "search", "function");
        for lang in [
            LangId::Php,
            LangId::C,
            LangId::Javascript,
            LangId::Typescript,
            LangId::Tsx,
            LangId::Rust,
            LangId::Java,
        ] {
            let a = RefAttribution {
                ref_lang: Some(lang),
                origin: RefOrigin::Unproven,
            };
            assert!(
                proves_survivor_origin(&a, &deleted, &defs),
                "{lang:?} は Python 定義へ識別子束縛できないので証明成立すべき"
            );
        }
        // 同一言語 (Python) の bare 参照は従来どおり証明不能 = blocking 維持
        let same_lang = RefAttribution {
            ref_lang: Some(LangId::Python),
            origin: RefOrigin::Unproven,
        };
        assert!(!proves_survivor_origin(&same_lang, &deleted, &defs));
        // 言語が判らない参照 (Android XML 等) も fail-closed
        let unknown = RefAttribution {
            ref_lang: None,
            origin: RefOrigin::Unproven,
        };
        assert!(!proves_survivor_origin(&unknown, &deleted, &defs));
        // 削除側が C ABI を公開する言語なら、ctypes/cinterop 経由で束縛され得るため証明しない
        let c_deleted = candidate("native/lib.c", "search", "function");
        for lang in [LangId::Python, LangId::Kotlin, LangId::Php, LangId::Ruby] {
            let a = RefAttribution {
                ref_lang: Some(lang),
                origin: RefOrigin::Unproven,
            };
            assert!(
                !proves_survivor_origin(&a, &c_deleted, &defs),
                "{lang:?} は C ABI シンボルを識別子として取り込み得るので blocking 維持すべき"
            );
        }
    }

    /// Python の属性アクセスのみの参照は、レシーバが削除モジュールでなければ証明成立。
    /// メソッド削除では属性アクセスが正当な参照形なので証明しない。
    #[test]
    fn proves_survivor_origin_python_attribute_access() {
        let defs: HashSet<String> = HashSet::new();
        let receivers = |names: &[&str]| {
            attr(RefOrigin::PythonAttributeAccess {
                receiver_module_names: Rc::new(
                    names
                        .iter()
                        .map(|s| (*s).to_string())
                        .collect::<HashSet<_>>(),
                ),
            })
        };
        // `re.search(...)` のみ → 削除した core.py の search とは無関係
        assert!(proves_survivor_origin(
            &receivers(&["re", "self"]),
            &candidate("scripts/core.py", "search", "function"),
            &defs
        ));
        // `core.search(...)` → 削除モジュールへの残存参照
        assert!(!proves_survivor_origin(
            &receivers(&["re", "core"]),
            &candidate("scripts/core.py", "search", "function"),
            &defs
        ));
        // `pkg/__init__.py` の削除はパッケージ名 `pkg` で参照される
        assert!(!proves_survivor_origin(
            &receivers(&["pkg"]),
            &candidate("pkg/__init__.py", "search", "function"),
            &defs
        ));
        // メソッド削除は属性アクセスこそが正当な参照形 → 証明しない (fail-closed)
        assert!(!proves_survivor_origin(
            &receivers(&["re"]),
            &candidate("scripts/core.py", "BM25.fit", "method"),
            &defs
        ));
        assert!(!proves_survivor_origin(
            &receivers(&["re"]),
            &candidate("scripts/core.py", "fit", "method"),
            &defs
        ));
    }

    #[test]
    fn python_module_name_uses_package_name_for_init() {
        assert_eq!(
            python_module_name("scripts/core.py").as_deref(),
            Some("core")
        );
        assert_eq!(
            python_module_name("pkg/__init__.py").as_deref(),
            Some("pkg")
        );
        assert_eq!(python_module_name("core.py").as_deref(), Some("core"));
    }

    fn python_facts_for(src: &str, symbol: &str) -> RefAttributionFacts {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ref.py");
        std::fs::write(&path, src).expect("write");
        let utf8 = camino::Utf8Path::from_path(&path).expect("utf8");
        analyze_python_facts(utf8, symbol)
    }

    /// 属性アクセスだけのファイルはレシーバ名を集め、bare 出現があれば証明を諦める。
    #[test]
    fn analyze_python_facts_detects_attribute_only_usage() {
        let facts = python_facts_for(
            "import re\n\ndef check(pw):\n    return re.search('x', pw)\n",
            "search",
        );
        let receivers = facts.python_attribute_receivers.expect("attribute only");
        assert!(receivers.contains("re"), "receivers={receivers:?}");

        // 別名 import はモジュール実名へ解決する
        let facts = python_facts_for(
            "import scripts.core as c\n\ndef run():\n    return c.search(1)\n",
            "search",
        );
        let receivers = facts.python_attribute_receivers.expect("attribute only");
        assert!(receivers.contains("core"), "receivers={receivers:?}");

        // ドット連鎖は直前の segment を実効モジュール名にする
        let facts = python_facts_for(
            "import scripts\n\ndef run():\n    return scripts.core.search(1)\n",
            "search",
        );
        let receivers = facts.python_attribute_receivers.expect("attribute only");
        assert!(receivers.contains("core"), "receivers={receivers:?}");

        // bare 呼び出しが混ざれば証明不能
        let facts = python_facts_for(
            "import re\n\ndef run(pw):\n    if re.search('x', pw):\n        return search(pw)\n    return None\n",
            "search",
        );
        assert!(facts.python_attribute_receivers.is_none());

        // from import の識別子出現も bare なので証明不能
        let facts = python_facts_for("from core import search\n", "search");
        assert!(facts.python_attribute_receivers.is_none());
    }
}
