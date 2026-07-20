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
pub(crate) enum RefAttribution {
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
    /// 証明不能。従来どおり残存参照として数える。
    Unproven,
}

/// `attr` が「削除ファイル `old_path` のシンボルではなく残存シンボル由来」と証明できるか。
/// `residual_def_paths` は同 bare name の残存定義ファイル集合 (repo 相対)。
pub(crate) fn proves_survivor_origin(
    attr: &RefAttribution,
    old_path: &str,
    residual_def_paths: &HashSet<String>,
) -> bool {
    match attr {
        RefAttribution::SelfDefined { sourced_candidates } => sourced_candidates
            .as_ref()
            .is_none_or(|sourced| !sourced.contains(old_path)),
        RefAttribution::ImportResolved { candidates } => {
            !candidates.contains(old_path)
                && candidates.iter().any(|c| residual_def_paths.contains(c))
        }
        RefAttribution::Unproven => false,
    }
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
    if facts.is_js_ts {
        if facts.has_unresolvable_import_binding {
            return RefAttribution::Unproven;
        }
        // from 句付き文で束縛 / 再輸出されている場合は同ファイル定義より優先して
        // import 解決で判定する (`export { x } from './deleted'` は同ファイル定義と
        // 共存でき、削除で壊れるため SelfDefined で証明してはならない)。
        if let Some(candidates) = &facts.local_import_candidates {
            return RefAttribution::ImportResolved {
                candidates: Rc::clone(candidates),
            };
        }
        if residual_def_paths.contains(ref_path) {
            return RefAttribution::SelfDefined {
                sourced_candidates: None,
            };
        }
        return RefAttribution::Unproven;
    }
    if facts.is_bash && residual_def_paths.contains(ref_path) {
        return RefAttribution::SelfDefined {
            sourced_candidates: facts.bash_sourced_candidates.clone(),
        };
    }
    RefAttribution::Unproven
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
    let Ok(lang) = LangId::from_path(utf8) else {
        return RefAttributionFacts::opaque();
    };
    match lang {
        LangId::Javascript | LangId::Typescript | LangId::Tsx => {
            analyze_js_ts_facts(utf8, lang, ref_path, symbol, external_pkgs)
        }
        LangId::Bash => analyze_bash_facts(utf8, ref_path),
        _ => RefAttributionFacts::opaque(),
    }
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

    #[test]
    fn proves_survivor_origin_matrix() {
        let defs: HashSet<String> = ["api/src/config.ts".to_string()].into();
        // 残存定義への import 解決 → 証明
        let attr = RefAttribution::ImportResolved {
            candidates: Rc::new(
                [
                    "api/src/config.ts".to_string(),
                    "api/src/config.js".to_string(),
                ]
                .into(),
            ),
        };
        assert!(proves_survivor_origin(&attr, "plugins/setup.mjs", &defs));
        // 候補に old_path が含まれる → 証明失敗 (削除ファイルへの残存参照)
        let attr = RefAttribution::ImportResolved {
            candidates: Rc::new(["plugins/setup.mjs".to_string()].into()),
        };
        assert!(!proves_survivor_origin(&attr, "plugins/setup.mjs", &defs));
        // 候補が残存定義と交差しない (re-export barrel 等) → 証明失敗
        let attr = RefAttribution::ImportResolved {
            candidates: Rc::new(["lib/barrel.ts".to_string()].into()),
        };
        assert!(!proves_survivor_origin(&attr, "plugins/setup.mjs", &defs));
        // 同ファイル定義 (TS/JS) → 証明
        let attr = RefAttribution::SelfDefined {
            sourced_candidates: None,
        };
        assert!(proves_survivor_origin(&attr, "plugins/setup.mjs", &defs));
        // bash: リテラル source が old_path を指す → 証明失敗
        let attr = RefAttribution::SelfDefined {
            sourced_candidates: Some(Rc::new(["scripts/deleted.sh".to_string()].into())),
        };
        assert!(!proves_survivor_origin(&attr, "scripts/deleted.sh", &defs));
        assert!(proves_survivor_origin(&attr, "scripts/other.sh", &defs));
        // 証明不能は常に false
        assert!(!proves_survivor_origin(
            &RefAttribution::Unproven,
            "x",
            &defs
        ));
    }
}
