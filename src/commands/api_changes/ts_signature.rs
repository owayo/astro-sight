//! TS/TSX/JS 固有のシグネチャ解析と互換 API 変更判定ヘルパー
//! (React HOC ラップ / object member / 末尾 optional 引数 / 引数なし→省略可能 destructured)。

use anyhow::Result;
use rayon::prelude::*;
use std::collections::{BTreeSet, HashMap, HashSet};

use crate::engine::parser;
use crate::models::review::CompatibleApiModification;

use super::super::git_input::git_show_blob;
use super::source_pair::{CompatibleModSite, SignatureSourceCache};
use super::{ApiRefIndex, has_blocking_value_usage, normalize_signature_whitespace};

/// TS/TSX/JS 全体を対象にする判定器の言語ゲート。
const TS_JS_LANGS: &[crate::language::LangId] = &[
    crate::language::LangId::Typescript,
    crate::language::LangId::Tsx,
    crate::language::LangId::Javascript,
];

/// トップレベル関数 / class method の AST 解決を要する判定器の言語ゲート (JS を含まない)。
const TS_ONLY_LANGS: &[crate::language::LangId] = &[
    crate::language::LangId::Typescript,
    crate::language::LangId::Tsx,
];

/// TypeScript の有限 literal union を決定的な集合へ正規化するための値。
///
/// string / number は raw token を保持する。`"x"` と `'x'`、`1` と `1.0` のような
/// 実行上は同値になりうる表記まで同一視すると escape / 数値正規化が必要になるため、
/// 現段階では保守的に別値として扱う。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum TsLiteralValue {
    String(String),
    Number(String),
    Boolean(bool),
    Null,
}

/// 同一ファイル内の `type` alias を有限 literal 集合へ評価する。
///
/// 対応範囲は literal / union / 冗長括弧 / 同一ファイル内の一意な alias chain のみ。
/// import 越し、非 literal、循環、多重宣言、parse error は `None` に倒し、呼び出し側が
/// blocking な `api.mod` を維持できるようにする。
fn eval_named_literal_union(
    root: tree_sitter::Node<'_>,
    source: &[u8],
    name: &str,
) -> Option<BTreeSet<TsLiteralValue>> {
    let value = unique_top_level_type_alias_value(root, source, name)?;
    let mut visiting = BTreeSet::new();
    visiting.insert(name.to_string());
    eval_literal_union(value, root, source, &mut visiting, 0)
}

fn eval_literal_union(
    node: tree_sitter::Node<'_>,
    root: tree_sitter::Node<'_>,
    source: &[u8],
    visiting: &mut BTreeSet<String>,
    depth: usize,
) -> Option<BTreeSet<TsLiteralValue>> {
    const MAX_ALIAS_DEPTH: usize = 32;
    if depth > MAX_ALIAS_DEPTH {
        return None;
    }

    match node.kind() {
        "parenthesized_type" => {
            let inner = node.named_child(0)?;
            eval_literal_union(inner, root, source, visiting, depth + 1)
        }
        "union_type" => {
            let mut values = BTreeSet::new();
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                values.extend(eval_literal_union(
                    child,
                    root,
                    source,
                    visiting,
                    depth + 1,
                )?);
            }
            (!values.is_empty()).then_some(values)
        }
        "literal_type" => {
            let literal = node.named_child(0)?;
            let value = match literal.kind() {
                "string" => TsLiteralValue::String(literal.utf8_text(source).ok()?.to_string()),
                "number" => TsLiteralValue::Number(literal.utf8_text(source).ok()?.to_string()),
                "true" => TsLiteralValue::Boolean(true),
                "false" => TsLiteralValue::Boolean(false),
                "null" => TsLiteralValue::Null,
                _ => return None,
            };
            Some(BTreeSet::from([value]))
        }
        "type_identifier" => {
            let alias = node.utf8_text(source).ok()?.to_string();
            if !visiting.insert(alias.clone()) {
                return None;
            }
            let value = unique_top_level_type_alias_value(root, source, &alias)?;
            let result = eval_literal_union(value, root, source, visiting, depth + 1);
            visiting.remove(&alias);
            result
        }
        _ => None,
    }
}

/// トップレベルに同名の `type_alias_declaration` がちょうど 1 件ある場合だけ宣言ノードを返す。
/// interface merge や同名多重宣言、import された alias は解決しない。
fn unique_top_level_type_alias_decl<'a>(
    root: tree_sitter::Node<'a>,
    source: &[u8],
    name: &str,
) -> Option<tree_sitter::Node<'a>> {
    let decls = collect_top_level_type_decls(root, source, name);
    let [decl] = decls.as_slice() else {
        return None;
    };
    (decl.kind() == "type_alias_declaration").then_some(*decl)
}

/// トップレベルに同名の `type_alias_declaration` がちょうど 1 件ある場合だけ RHS を返す。
fn unique_top_level_type_alias_value<'a>(
    root: tree_sitter::Node<'a>,
    source: &[u8],
    name: &str,
) -> Option<tree_sitter::Node<'a>> {
    unique_top_level_type_alias_decl(root, source, name)?.child_by_field_name("value")
}

/// 型エイリアスの型パラメータ列 (`<T extends string = "x">`) を token 列として返す。
/// 型パラメータが無ければ空の Vec。
///
/// **RHS だけを比べて降格してはいけない**: `type Category<T extends string> = "a" | "b"` を
/// `type Category = "a" | "b"` にすると RHS は不変だが、利用側の `Category<"x">` は
/// `TS2315: Type 'Category' is not generic.` で落ちる。逆向き (型パラメータの追加) も
/// `TS2314: Generic type 'Mode' requires 1 type argument(s).` で落ちる。どちらも公開契約の
/// 破壊なので、fail-closed 規約どおり blocking な api.mod を維持しなければならない。
///
/// 個数だけでなく constraint / default / variance modifier (`in` / `out`) / `const` も
/// 契約なので、列全体を比較する。`T` → `U` の alpha rename も blocking に残るが、これは
/// false negative を避ける安全側 (降格できるはずのものが降格しないだけ)。
///
/// 比較単位は **AST の leaf token (kind + 元テキスト) の列**。整形差 (トークン間の空白・
/// 改行・コメント) だけを無視し、トークン境界は保つ。
/// **テキストから空白を除去する正規化にしてはならない** — 2 つの壊れ方がある:
/// (a) 文字列リテラル**内**の空白まで落ちて `T extends "a b"` と `T extends "ab"` が
/// 同一になる (`Category<"a b">` が変更後に型エラーになるのに降格してしまう)、
/// (b) トークン境界が消えて `<T extends string>` と `<Textendsstring>` が衝突する。
fn type_alias_type_parameter_tokens(
    root: tree_sitter::Node<'_>,
    source: &[u8],
    name: &str,
) -> Option<Vec<(String, String)>> {
    let decl = unique_top_level_type_alias_decl(root, source, name)?;
    let Some(params) = decl.child_by_field_name("type_parameters") else {
        return Some(Vec::new());
    };
    let mut tokens = Vec::new();
    collect_leaf_tokens(params, source, &mut tokens)?;
    Some(tokens)
}

/// ノード配下の leaf token を `(kind, 元テキスト)` の列として集める。
/// `extra` ノード (コメント等) は整形の一部なので読み飛ばす。
///
/// 走査は単一 `TreeCursor` の反復 pre-order (`walk_refs` と同じ方式)。constraint / default
/// には任意に深い型を書けるため、再帰にするとスタックオーバーフローし得る。
fn collect_leaf_tokens(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    out: &mut LeafTokens,
) -> Option<()> {
    let mut cursor = node.walk();
    loop {
        let current = cursor.node();
        let skip_subtree = current.is_extra() || current.kind() == "comment";
        if !skip_subtree {
            if current.child_count() == 0 {
                let text =
                    std::str::from_utf8(source.get(current.start_byte()..current.end_byte())?)
                        .ok()?;
                out.push((current.kind().to_string(), text.to_string()));
            } else if cursor.goto_first_child() {
                continue;
            }
        }
        // 兄弟へ進む。無ければ親へ戻り、起点まで戻ったら終了。
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return Some(());
            }
            if cursor.node().id() == node.id() {
                return Some(());
            }
        }
    }
}

/// TypeScript / TSX の公開 type alias が、同一ファイル内で証明できる有限 literal 集合として
/// old/new 同値なら `compatible_modified` へ降格する。
///
/// 値集合の拡大・縮小・置換、import 越し alias、generic / conditional / intersection 等は
/// 一切降格せず、従来どおり blocking な `api.mod` を維持する。
///
/// **RHS の同値だけでは足りない**: 型パラメータ列 (`<T extends string>`) は RHS の外にある
/// 公開契約なので、old/new で一致することも要求する (`type_alias_type_parameter_tokens`)。
pub(crate) fn detect_equivalent_literal_union_alias_compatible_mod(
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache,
) -> Option<CompatibleApiModification> {
    let lang = site.lang_in(TS_ONLY_LANGS)?;
    if site.kind != "type" {
        return None;
    }
    let src = sources.get(site)?;
    let (old_tree, new_tree) = src.parse_pair(lang)?;
    if old_tree.root_node().has_error() || new_tree.root_node().has_error() {
        return None;
    }
    let old_params = type_alias_type_parameter_tokens(old_tree.root_node(), &src.old, site.name)?;
    let new_params = type_alias_type_parameter_tokens(new_tree.root_node(), &src.new, site.name)?;
    if old_params != new_params {
        return None;
    }
    let old_values = eval_named_literal_union(old_tree.root_node(), &src.old, site.name)?;
    let new_values = eval_named_literal_union(new_tree.root_node(), &src.new, site.name)?;
    (old_values == new_values).then(|| site.compatible("equivalent_literal_union_alias"))
}

/// TS/TSX/JS の exported component を `memo` / `forwardRef` 等の HOC でラップしただけの
/// api.mod を互換変更 (`react_component_wrapper`) として判定する。
///
/// `export function X(props: T) {}` → `export const X = memo(function X(props: T) {})` の
/// ように宣言種別が変わると signature 文字列が変化して api.mod になるが、export 名・props
/// 型・JSX 利用互換性が維持されるなら公開契約は不変。次をすべて満たすとき降格する:
/// - 言語が TS / TSX / JS
/// - new 側が `memo` / `forwardRef` (`React.*` 含む) でラップされている
/// - old / new 双方から `function <name>(<params>)` の引数リストを抽出でき正規化一致する
/// - 引数に型注釈がある (型なしは JSX 互換を保証できないため除外)
/// - 当該シンボルに値利用参照 (`X(...)` / `new X` / `typeof X` / `X.foo` / `X[...]`) が無い
///
/// 抽出失敗・型注釈なし・参照解析失敗・判定不能な参照は None を返し blocking を維持する
/// (false negative 回避)。
pub(crate) fn detect_react_wrapper_compatible_mod(
    index: &ApiRefIndex,
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache,
) -> Option<CompatibleApiModification> {
    let lang = site.lang_in(TS_JS_LANGS)?;
    // new 側が memo / forwardRef でラップされていること (単なる function 本体変更は対象外)。
    if !new_sig_has_react_wrapper(site.new_sig) {
        return None;
    }
    // old 側は非 wrapper (function 宣言等) であること。wrapper-to-wrapper の変更
    // (`forwardRef<HTMLDivElement, P>` → `forwardRef<HTMLButtonElement, P>` 等) は ref 型や
    // generic の差分を取りこぼすため対象外 (codex 指摘)。
    if new_sig_has_react_wrapper(site.old_sig) {
        return None;
    }
    // old は base リビジョン、new は working tree からソースを再取得して props 型を AST 抽出
    // する。signature 文字列は const の先頭行 fallback で複数行 destructured props の型注釈を
    // 取りこぼすため、ソース再パースで比較する (codex 設計合意)。
    let src = sources.get(site)?;
    // old / new 双方の第1引数 (props) の型注釈を抽出して一致を要求する。
    let old_props = extract_component_props_type(&src.old, lang, site.name)?;
    let new_props = extract_component_props_type(&src.new, lang, site.name)?;
    if old_props != new_props {
        return None;
    }
    // 値利用 (呼び出し / typeof / member / new / indexed) が残れば MemoExoticComponent 化で
    // 壊れ得るため blocking 維持。
    if has_blocking_value_usage(index, site.name) {
        return None;
    }
    Some(site.compatible("react_component_wrapper"))
}

/// new 側 signature が `memo(` / `forwardRef(` / `React.memo(` / `React.forwardRef(` で
/// ラップされているか (identifier 境界を確認し `somememo` 等の部分一致を弾く)。
pub(crate) fn new_sig_has_react_wrapper(sig: &str) -> bool {
    let bytes = sig.as_bytes();
    for kw in ["memo", "forwardRef"] {
        let kb = kw.as_bytes();
        let mut i = 0;
        while i + kb.len() <= bytes.len() {
            if &bytes[i..i + kb.len()] == kb {
                let before_ok = i == 0 || {
                    let p = bytes[i - 1];
                    // `React.memo` の `.` は許容、識別子継続文字は不可
                    !(p.is_ascii_alphanumeric() || p == b'_' || p == b'$')
                };
                let after = sig[i + kb.len()..].trim_start();
                if before_ok && after.starts_with('(') {
                    return true;
                }
            }
            i += 1;
        }
    }
    false
}

/// TS/TSX/JS ソースから、トップレベル exported な `name` のコンポーネント関数の第1引数
/// (props) の型注釈テキスト (例 `: ScheduleItemProps`、whitespace 正規化済み) を抽出する。
/// `export function name(p: T)` / `export const name = memo(function(p: T))` /
/// `forwardRef((p: T, ref) => ...)` に対応し、宣言 subtree の最初の formal_parameters を見る。
/// 宣言が見つからない / 同名宣言が複数 / 第1引数に型注釈が無い / parse 失敗なら None
/// (呼び出し側で blocking 維持)。
pub(crate) fn extract_component_props_type(
    source: &[u8],
    lang_id: crate::language::LangId,
    name: &str,
) -> Option<String> {
    let tree = parser::parse_source(source, lang_id).ok()?;
    let root = tree.root_node();
    let decls = find_toplevel_decls_named(root, name, source);
    if decls.len() != 1 {
        return None;
    }
    let params = first_descendant_formal_parameters(decls[0])?;
    first_param_type_text(params, source)
}

/// program 直下 (export_statement のラップを潜る) で `name` を宣言する function_declaration
/// または variable_declarator ノードを集める。
pub(crate) fn find_toplevel_decls_named<'a>(
    root: tree_sitter::Node<'a>,
    name: &str,
    source: &[u8],
) -> Vec<tree_sitter::Node<'a>> {
    let mut result = Vec::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        let decl = if child.kind() == "export_statement" {
            match child.named_child(0) {
                Some(d) => d,
                None => continue,
            }
        } else {
            child
        };
        match decl.kind() {
            "function_declaration" | "generator_function_declaration" => {
                if node_field_name_eq(decl, name, source) {
                    result.push(decl);
                }
            }
            "lexical_declaration" | "variable_declaration" => {
                let mut c2 = decl.walk();
                for d in decl.named_children(&mut c2) {
                    if d.kind() == "variable_declarator" && node_field_name_eq(d, name, source) {
                        result.push(d);
                    }
                }
            }
            _ => {}
        }
    }
    result
}

/// ノードの `name` フィールドのテキストが `name` と一致するか。
pub(crate) fn node_field_name_eq(node: tree_sitter::Node, name: &str, source: &[u8]) -> bool {
    node.child_by_field_name("name")
        .and_then(|n| n.utf8_text(source).ok())
        == Some(name)
}

/// `node` の subtree を深さ優先で走査し最初の formal_parameters ノードを返す。
pub(crate) fn first_descendant_formal_parameters(
    node: tree_sitter::Node,
) -> Option<tree_sitter::Node> {
    if node.kind() == "formal_parameters" {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(found) = first_descendant_formal_parameters(child) {
            return Some(found);
        }
    }
    None
}

/// formal_parameters の第1引数の型注釈テキスト (whitespace 正規化済み) を返す。
/// 第1引数が required/optional_parameter で `type` フィールドを持つときのみ Some。
/// 型注釈が無い (JS 風 identifier param 等) / 引数なしなら None。
pub(crate) fn first_param_type_text(params: tree_sitter::Node, source: &[u8]) -> Option<String> {
    let mut cursor = params.walk();
    let first = params.named_children(&mut cursor).next()?;
    match first.kind() {
        "required_parameter" | "optional_parameter" => {
            let type_node = first.child_by_field_name("type")?;
            let text = type_node.utf8_text(source).ok()?;
            Some(text.split_whitespace().collect::<Vec<_>>().join(" "))
        }
        _ => None,
    }
}

/// 参照行 `ctx` 内の `name` 出現がすべて JSX タグ利用 (`<X` / `</X`) かを判定する。
/// 値利用 (`X(` 呼び出し / `X.` / `X[` / `new X` / `typeof X`) や JSX でない裸の出現を
/// 含むなら false (= blocking 側に倒す)。
pub(crate) fn ctx_usage_is_jsx_or_safe(ctx: &str, name: &str) -> bool {
    let bytes = ctx.as_bytes();
    let nb = name.as_bytes();
    if nb.is_empty() {
        return false;
    }
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$';
    let mut i = 0;
    let mut saw_occurrence = false;
    while i + nb.len() <= bytes.len() {
        if &bytes[i..i + nb.len()] == nb {
            let before = if i == 0 { None } else { Some(bytes[i - 1]) };
            let after = bytes.get(i + nb.len()).copied();
            let before_boundary = before.is_none_or(|b| !is_ident(b));
            let after_boundary = after.is_none_or(|b| !is_ident(b));
            if before_boundary && after_boundary {
                saw_occurrence = true;
                let next_non_ws = ctx[i + nb.len()..].trim_start().as_bytes().first().copied();
                let is_call = next_non_ws == Some(b'(');
                let is_member = next_non_ws == Some(b'.') || next_non_ws == Some(b'[');
                // 直前の識別子トークンを取る (空白だけでなく `(` `=` 等の非識別子文字でも
                // 区切る)。`memo(function NAME` のように `(` 直後に関数キーワードが来るケースを
                // 正しく拾うため split_whitespace ではなく識別子境界で分割する。
                let last_ident = ctx[..i]
                    .rsplit(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$'))
                    .find(|s| !s.is_empty())
                    .unwrap_or("");
                let is_typeof = last_ident == "typeof";
                let is_new = last_ident == "new";
                // 宣言キーワード直後の出現は定義 (変数宣言名 / named function expression 名 /
                // class 名) であり値利用でない。`export const X = memo(function X(...))` の
                // `const X` と内側 `function X` の両方がこれに当たる。
                let is_decl = matches!(last_ident, "const" | "let" | "var" | "function" | "class");
                if !is_decl && (is_call || is_member || is_typeof || is_new) {
                    return false;
                }
                let is_jsx = before == Some(b'<') || (i >= 2 && &bytes[i - 2..i] == b"</");
                if !is_jsx && !is_decl {
                    // JSX でも宣言でも値利用でもない裸の出現は判定不能 → 安全側 (blocking)
                    return false;
                }
            }
        }
        i += 1;
    }
    saw_occurrence
}

/// TS/TSX/JS の exported object (`export const X = { ... }`) のプロパティ削除を互換変更
/// (`unused_object_members`) として判定する。
///
/// initializer の object literal を flat object または homogeneous record として抽出し、
/// 削除された schema キーが無い (追加のみ) か、削除された schema キーすべてが repo 全体で
/// member access (`.key` / `['key']` / `["key"]`) として参照されていない場合に降格する。
/// 値のみ変更 / spread / computed key / mixed shape / record schema 不揃い / object でない /
/// 抽出不能 / 同名複数宣言 / 削除キーの参照残存はすべて blocking 維持 (false negative 回避)。
pub(crate) fn detect_object_members_compatible_mod(
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache,
) -> Option<CompatibleApiModification> {
    let lang = site.lang_in(TS_JS_LANGS)?;
    let src = sources.get(site)?;
    let old_keys = extract_object_member_keys(&src.old, lang, site.name)?;
    let new_keys = extract_object_member_keys(&src.new, lang, site.name)?;
    if old_keys.record_keys.is_some() != new_keys.record_keys.is_some() {
        return None;
    }
    // 型注釈と `as` / `satisfies` は object literal の外にある公開契約。member キーだけで
    // 降格すると、`cfg: { alpha: number }` → `cfg: { alpha: number | string; beta: number }`
    // のように「無関係なキー追加」が同居した型変更が後方互換に見えてしまう (tsc は TS2322 で
    // 落ちる)。`as const` の付け外しや `satisfies T` の型変更も同じ理由で blocking に残す。
    if old_keys.declarator_type != new_keys.declarator_type
        || old_keys.wrappers != new_keys.wrappers
    {
        return None;
    }
    // record entry を包む wrapper も同様。トップレベルだけ見ると
    // `{ google: { alpha: 1 } as const }` → `{ google: { alpha: 1, beta: 2 } }` を
    // 「キー追加のみ」として降格してしまう。両側に残る record key について比較する
    // (片側にしか無い key は追加/削除であり、その可否は別途判定する)。
    for (key, old_wrappers) in &old_keys.entry_wrappers {
        if let Some(new_wrappers) = new_keys.entry_wrappers.get(key)
            && new_wrappers != old_wrappers
        {
            return None;
        }
    }
    let has_added_member = new_keys
        .member_keys
        .difference(&old_keys.member_keys)
        .next()
        .is_some();
    let has_added_record_entry = match (&old_keys.record_keys, &new_keys.record_keys) {
        (Some(old_record), Some(new_record)) => {
            // record entry の削除は dynamic access (`config[id]`) を静的保証できないため blocking。
            if old_record.difference(new_record).next().is_some() {
                return None;
            }
            new_record.difference(old_record).next().is_some()
        }
        (None, None) => false,
        _ => return None,
    };
    let removed_members: Vec<&String> = old_keys
        .member_keys
        .difference(&new_keys.member_keys)
        .collect();
    if removed_members.is_empty() && !has_added_member && !has_added_record_entry {
        return None;
    }
    // 両側に残存するキーの値が変わっていれば破壊的なので blocking 維持。
    // キー集合の差分だけを見ていると、`{a: () => {}}` → `{a: 42, b: () => {}}` のように
    // 「既存キーの値の差し替え」と「無関係なキー追加」が同一 diff に同居した場合に
    // 追加のみの変更として降格してしまう (呼び出し側の `a()` が実行時に壊れる)。
    // 片側にしか無いキーは追加/削除であり、その可否は別途 removed_members 側で判定する。
    for (key, old_value) in &old_keys.entry_values {
        if let Some(new_value) = new_keys.entry_values.get(key)
            && new_value != old_value
        {
            return None;
        }
    }
    // 削除された schema キー (old にあって new にない)。いずれかへの member access が repo
    // 全体で残っていれば破壊的なので blocking 維持。キーごとに全ファイルを再走査すると
    // O(削除キー数 × ファイル数) になるため、対象キーを 1 個の AC と HashSet にまとめ、
    // 各ファイルの read / parse / AST walk を最大 1 回に集約する。
    let removed_member_set: HashSet<&str> =
        removed_members.iter().map(|key| key.as_str()).collect();
    if member_access_keys_have_ref(site.dir, &removed_member_set) {
        return None;
    }
    Some(site.compatible("unused_object_members"))
}

/// TS/TSX のトップレベル exported function / class method で、末尾 optional/default
/// 引数追加だけを compatible_modified (`trailing_optional_params`) として判定する。
///
/// 次をすべて満たす場合だけ降格する:
/// - 関数シンボル (bare 名) はトップレベル関数として old/new とも一意に取得できる
/// - method シンボル (`Class.method` qualname) はトップレベル (export 文直下含む) の
///   class 宣言が一意で、class body 直下の同名 callable member が method_definition
///   1 件のみ (overload signature / abstract signature を含め同名複数なら不成立)
/// - 関数名・型パラメータ・戻り値・modifier など parameters 外の signature が不変
/// - 既存引数の順序・型・optional/default 指定が不変
/// - 追加された末尾引数がすべて optional (`?`) または default value 付き
///
/// const arrow function / nested class / import 型の解決などは対象外にして blocking を
/// 維持する。false negative (破壊的変更の見逃し) を避けるため、AST 取得や git show に
/// 失敗した場合も None。
pub(crate) fn detect_trailing_optional_params_compatible_mod(
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache,
) -> Option<CompatibleApiModification> {
    with_resolved_ts_fn_pair(site, sources, |old_fn, old_source, new_fn, new_source| {
        let old_parts = ts_function_signature_parts(old_fn, old_source)?;
        let new_parts = ts_function_signature_parts(new_fn, new_source)?;

        if old_parts.head != new_parts.head || old_parts.tail != new_parts.tail {
            return None;
        }
        if !ts_params_prefix_same_with_optional_tail(&old_parts.params, &new_parts.params) {
            return None;
        }
        Some(())
    })?;

    Some(site.compatible("trailing_optional_params"))
}

/// TS/TSX のトップレベル exported function / class method で、引数の inline object type
/// literal へ optional プロパティを追加しただけの変更を compatible_modified
/// (`optional_object_props`) として判定する
/// (Issue 2026-07-20-api-mod-additive-optional-param-overreport)。
///
/// `decide(input: { a: number })` → `decide(input: { a: number; cap?: number })` のような
/// 変更は、引数 (反変) 位置の受理集合を広げるだけで既存呼び出しを壊さない。
///
/// 次をすべて満たす場合だけ降格する:
/// - シンボル解決条件は trailing_optional_params と同一 (トップレベル関数 / class method
///   が old/new とも一意)
/// - 関数名・型パラメータ・戻り値・modifier など parameters 外の signature が不変
/// - 引数の個数が不変で、各引数はテキスト不変、または「引数名・default 値が不変で
///   type annotation が object type literal 同士、既存メンバーがテキスト完全一致で残り、
///   追加メンバーがすべて optional (`?`) の property_signature」
///
/// ネストした object type の内部変更は既存メンバーのテキスト不一致になるため対象外
/// (第一階層の追加のみ許容)。メンバー削除・型変更・必須メンバー追加は blocking を維持する。
pub(crate) fn detect_optional_object_props_compatible_mod(
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache,
) -> Option<CompatibleApiModification> {
    with_resolved_ts_fn_pair(site, sources, |old_fn, old_source, new_fn, new_source| {
        let old_parts = ts_function_signature_parts(old_fn, old_source)?;
        let new_parts = ts_function_signature_parts(new_fn, new_source)?;
        if old_parts.head != new_parts.head || old_parts.tail != new_parts.tail {
            return None;
        }

        let old_params_node = old_fn.child_by_field_name("parameters")?;
        let new_params_node = new_fn.child_by_field_name("parameters")?;
        let mut old_cursor = old_params_node.walk();
        let old_children: Vec<tree_sitter::Node> =
            old_params_node.named_children(&mut old_cursor).collect();
        let mut new_cursor = new_params_node.walk();
        let new_children: Vec<tree_sitter::Node> =
            new_params_node.named_children(&mut new_cursor).collect();
        if old_children.len() != new_children.len() {
            return None;
        }

        let mut any_extension = false;
        for (old_param, new_param) in old_children.iter().zip(new_children.iter()) {
            let old_text = node_normalized_text(*old_param, old_source)?;
            let new_text = node_normalized_text(*new_param, new_source)?;
            if old_text == new_text {
                continue;
            }
            if !ts_param_pair_is_optional_object_extension(
                *old_param, old_source, *new_param, new_source,
            ) {
                return None;
            }
            any_extension = true;
        }
        any_extension.then_some(())
    })?;

    Some(site.compatible("optional_object_props"))
}

/// 「object type literal 引数へ**必須**プロパティを追加しただけ」の signature 変更で、
/// 追加された必須プロパティ名と対象引数の位置を返す。
///
/// これは互換変更**ではない** (呼び出し側は必ず新プロパティを渡す必要がある) ので
/// `compatible_modified` には使わない。用途は `is_modified_closed_in_diff` の追加証拠で、
/// 「呼び出し式は無変更だが、渡している共有 const の定義側に同一 diff 内で当該プロパティが
/// 追加されている」= 呼び出し側は追随済み、を判定するための入力になる
/// (Issue 2026-08-05-api-mod-callers-updated-indirectly のパターン C)。
///
/// 次をすべて満たす場合だけ `Some` を返す (それ以外は None = 追加証拠を使わない):
/// - シンボル解決条件は他の TS 判定器と同一 (トップレベル関数 / class method が old/new とも一意)
/// - 関数名・型パラメータ・戻り値・modifier など parameters 外の signature が不変
/// - 引数の個数が不変で、**テキストが変わった引数はちょうど 1 つ**
/// - その引数は名前・default 値・required/optional の別が不変で、type annotation が
///   object type literal 同士、既存メンバーがテキスト完全一致で残り、追加メンバーが
///   すべて**非** optional の `property_signature` (= 必須プロパティ追加のみ)
pub(crate) struct AddedRequiredObjectProps {
    /// テキストが変わった引数の 0-indexed 位置。
    pub(crate) param_index: usize,
    /// 新たに必須になったプロパティ名。
    pub(crate) names: Vec<String>,
}

pub(crate) fn detect_added_required_object_props(
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache,
) -> Option<AddedRequiredObjectProps> {
    with_resolved_ts_fn_pair(site, sources, |old_fn, old_source, new_fn, new_source| {
        let old_parts = ts_function_signature_parts(old_fn, old_source)?;
        let new_parts = ts_function_signature_parts(new_fn, new_source)?;
        if old_parts.head != new_parts.head || old_parts.tail != new_parts.tail {
            return None;
        }

        let old_params_node = old_fn.child_by_field_name("parameters")?;
        let new_params_node = new_fn.child_by_field_name("parameters")?;
        let mut old_cursor = old_params_node.walk();
        let old_children: Vec<tree_sitter::Node> =
            old_params_node.named_children(&mut old_cursor).collect();
        let mut new_cursor = new_params_node.walk();
        let new_children: Vec<tree_sitter::Node> =
            new_params_node.named_children(&mut new_cursor).collect();
        if old_children.len() != new_children.len() {
            return None;
        }

        let mut changed: Option<(usize, Vec<String>)> = None;
        for (ix, (old_param, new_param)) in old_children.iter().zip(new_children.iter()).enumerate()
        {
            let old_text = node_normalized_text(*old_param, old_source)?;
            let new_text = node_normalized_text(*new_param, new_source)?;
            if old_text == new_text {
                continue;
            }
            // 変更された引数が 2 つ以上あると「どの引数の追加プロパティか」を一意に
            // 決められないので追加証拠に使わない (blocking 維持)。
            if changed.is_some() {
                return None;
            }
            let names = ts_param_pair_added_required_prop_names(
                *old_param, old_source, *new_param, new_source,
            )?;
            changed = Some((ix, names));
        }
        let (param_index, names) = changed?;
        (!names.is_empty()).then_some(AddedRequiredObjectProps { param_index, names })
    })
}

/// 引数ペアが「inline object type literal への**必須**プロパティ追加だけ」かを判定し、
/// 追加された必須プロパティ名を返す。引数の kind (required/optional)・名前 (pattern)・
/// default 値は不変であること。既存メンバーの削除・変更、optional プロパティの追加が
/// 混ざっていれば None (追加証拠に使わない = blocking 維持)。
fn ts_param_pair_added_required_prop_names(
    old_param: tree_sitter::Node<'_>,
    old_source: &[u8],
    new_param: tree_sitter::Node<'_>,
    new_source: &[u8],
) -> Option<Vec<String>> {
    if old_param.kind() != new_param.kind()
        || !matches!(
            old_param.kind(),
            "required_parameter" | "optional_parameter"
        )
    {
        return None;
    }
    let (old_pattern, new_pattern) = (
        old_param.child_by_field_name("pattern")?,
        new_param.child_by_field_name("pattern")?,
    );
    if node_normalized_text(old_pattern, old_source)
        != node_normalized_text(new_pattern, new_source)
    {
        return None;
    }
    let old_value = old_param
        .child_by_field_name("value")
        .and_then(|n| node_normalized_text(n, old_source));
    let new_value = new_param
        .child_by_field_name("value")
        .and_then(|n| node_normalized_text(n, new_source));
    if old_value != new_value {
        return None;
    }
    let (old_ty, new_ty) = (
        ts_param_object_type(old_param)?,
        ts_param_object_type(new_param)?,
    );
    ts_object_type_added_required_members(old_ty, old_source, new_ty, new_source)
}

/// old object_type の全メンバーが new にテキスト一致で残り、new の追加メンバーがすべて
/// 必須 (`?` なし) の property_signature なら、その名前一覧を返す。
/// メンバー削除・変更、optional 追加の混在は None。
fn ts_object_type_added_required_members(
    old_ty: tree_sitter::Node<'_>,
    old_source: &[u8],
    new_ty: tree_sitter::Node<'_>,
    new_source: &[u8],
) -> Option<Vec<String>> {
    use std::collections::HashMap;
    let mut new_members: HashMap<String, Vec<tree_sitter::Node>> = HashMap::new();
    let mut cursor = new_ty.walk();
    for member in new_ty.named_children(&mut cursor) {
        new_members
            .entry(node_normalized_text(member, new_source)?)
            .or_default()
            .push(member);
    }
    let mut cursor = old_ty.walk();
    for member in old_ty.named_children(&mut cursor) {
        let text = node_normalized_text(member, old_source)?;
        match new_members.get_mut(&text) {
            Some(nodes) if !nodes.is_empty() => {
                nodes.pop();
            }
            // 既存メンバーの削除・変更があれば「必須プロパティ追加のみ」ではない
            _ => return None,
        }
    }
    // 出力順を決定論的にするため、追加メンバーはソース上の出現順に並べる。
    let mut added: Vec<tree_sitter::Node> = new_members.values().flatten().copied().collect();
    added.sort_by_key(tree_sitter::Node::start_byte);
    let mut names = Vec::with_capacity(added.len());
    for member in added {
        if member.kind() != "property_signature" || ts_property_signature_is_optional(member) {
            return None;
        }
        let key = member.child_by_field_name("name")?;
        names.push(object_key_text(key, new_source)?);
    }
    Some(names)
}

/// TSX/TS の exported function component へ `async` キーワードを追加しただけの変更を
/// compatible_modified (`async_jsx_component`) として判定する
/// (Issue 2026-07-20-react-rsc-async-component-impact-classification)。
///
/// React Server Component 規約 (Next.js App Router 等) では、async server component も
/// 同期 component も `<Foo />` の JSX 記法で呼び出され、async 化しても呼び出し側の
/// 書き換えは不要。次をすべて満たす場合だけ降格する:
/// - トップレベル関数 (bare 名) が old/new とも一意に解決できる
/// - 変更が `async` キーワードの追加のみ (params / 戻り値型など他の signature は不変)
/// - 変更後ファイルに `"use client"` directive が無い (Client Component の async 化は
///   React ランタイムエラーになる破壊的変更のため blocking 維持)
/// - repo 内の参照が JSX タグ利用 / import / re-export / 定義のみ (関数呼び出し
///   `Foo()` は戻り値が Promise になり await が必要になるため blocking 維持)
pub(crate) fn detect_async_jsx_component_compatible_mod(
    index: &ApiRefIndex,
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache,
) -> Option<CompatibleApiModification> {
    // class method は RSC の関数コンポーネント規約外なので対象にしない。
    if site.kind != "function" || site.name.contains('.') {
        return None;
    }
    with_resolved_ts_fn_pair(site, sources, |old_fn, old_source, new_fn, new_source| {
        let old_parts = ts_function_signature_parts(old_fn, old_source)?;
        let new_parts = ts_function_signature_parts(new_fn, new_source)?;
        if old_parts.params != new_parts.params || old_parts.tail != new_parts.tail {
            return None;
        }
        if !head_is_async_addition(&old_parts.head, &new_parts.head) {
            return None;
        }
        // 変更後が Client Component なら async 化は破壊的変更。
        if ts_module_has_use_client_directive(module_root(new_fn), new_source) {
            return None;
        }
        Some(())
    })?;
    // 値利用 (`Foo()` / `Foo.x` / `new Foo` 等) や判定不能な参照が残れば blocking 維持。
    if has_blocking_value_usage(index, site.name) {
        return None;
    }
    Some(site.compatible("async_jsx_component"))
}

/// `new_head` から `async ` を 1 箇所取り除くと `old_head` に一致するか
/// (= 変更が async キーワード追加のみか)。head は whitespace 正規化済み前提。
fn head_is_async_addition(old_head: &str, new_head: &str) -> bool {
    let Some(pos) = new_head.find("async ") else {
        return false;
    };
    // `async` が識別子の一部 (`myasync` 等) でないことを確認する。
    if pos > 0 {
        let before = new_head.as_bytes()[pos - 1];
        if before.is_ascii_alphanumeric() || before == b'_' || before == b'$' {
            return false;
        }
    }
    let stripped = format!("{}{}", &new_head[..pos], &new_head[pos + "async ".len()..]);
    stripped == old_head
}

/// ノードから module root (program ノード) まで遡る。
fn module_root(mut node: tree_sitter::Node<'_>) -> tree_sitter::Node<'_> {
    while let Some(parent) = node.parent() {
        node = parent;
    }
    node
}

/// module 先頭の directive prologue に `"use client"` があるか。
fn ts_module_has_use_client_directive(root: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        match child.kind() {
            "comment" => continue,
            "expression_statement" => {
                let mut inner = child.walk();
                let Some(string_node) = child
                    .named_children(&mut inner)
                    .find(|n| n.kind() == "string")
                else {
                    return false; // directive prologue 終了
                };
                let text = string_node.utf8_text(source).unwrap_or("");
                if text.trim_matches(|c| c == '"' || c == '\'') == "use client" {
                    return true;
                }
                // "use strict" 等の他 directive は読み飛ばして続行
            }
            _ => return false,
        }
    }
    false
}

/// TS/TSX の compatible 判定で共通の「old/new 両ツリーから対象関数ノードを一意解決する」
/// 前段。`check` に解決済みノードとソースを渡し、その結果を返す。解決不能 (git show 失敗 /
/// parse 失敗 / シンボル非一意) は None = blocking 維持。
fn with_resolved_ts_fn_pair<T>(
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache,
    check: impl FnOnce(tree_sitter::Node<'_>, &[u8], tree_sitter::Node<'_>, &[u8]) -> Option<T>,
) -> Option<T> {
    let lang = site.lang_in(TS_ONLY_LANGS)?;
    // 対象は「bare 名のトップレベル関数」または「Class.method の 2 要素 qualname メソッド」。
    let class_method = match site.kind {
        "function" if !site.name.contains('.') => None,
        "method" => Some(split_two_segment_qualname(site.name)?),
        _ => return None,
    };
    let src = sources.get(site)?;
    let (old_tree, new_tree) = src.parse_pair(lang)?;

    let (old_fn, new_fn) = match class_method {
        None => (
            find_top_level_function_by_name(old_tree.root_node(), &src.old, site.name)?,
            find_top_level_function_by_name(new_tree.root_node(), &src.new, site.name)?,
        ),
        Some((class_name, method_name)) => (
            find_unique_top_level_class_method(
                old_tree.root_node(),
                &src.old,
                class_name,
                method_name,
            )?,
            find_unique_top_level_class_method(
                new_tree.root_node(),
                &src.new,
                class_name,
                method_name,
            )?,
        ),
    };
    check(old_fn, &src.old, new_fn, &src.new)
}

/// ノードのソーステキストを whitespace 正規化して返す。
fn node_normalized_text(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    source
        .get(node.start_byte()..node.end_byte())
        .map(normalize_signature_whitespace)
}

/// 引数ペアが「inline object type literal への optional プロパティ追加だけ」の互換拡張かを
/// 判定する。引数の kind (required/optional)・名前 (pattern)・default 値は不変であること。
fn ts_param_pair_is_optional_object_extension(
    old_param: tree_sitter::Node<'_>,
    old_source: &[u8],
    new_param: tree_sitter::Node<'_>,
    new_source: &[u8],
) -> bool {
    if old_param.kind() != new_param.kind()
        || !matches!(
            old_param.kind(),
            "required_parameter" | "optional_parameter"
        )
    {
        return false;
    }
    let (Some(old_pattern), Some(new_pattern)) = (
        old_param.child_by_field_name("pattern"),
        new_param.child_by_field_name("pattern"),
    ) else {
        return false;
    };
    if node_normalized_text(old_pattern, old_source)
        != node_normalized_text(new_pattern, new_source)
    {
        return false;
    }
    let old_value = old_param
        .child_by_field_name("value")
        .and_then(|n| node_normalized_text(n, old_source));
    let new_value = new_param
        .child_by_field_name("value")
        .and_then(|n| node_normalized_text(n, new_source));
    if old_value != new_value {
        return false;
    }
    let (Some(old_ty), Some(new_ty)) = (
        ts_param_object_type(old_param),
        ts_param_object_type(new_param),
    ) else {
        return false;
    };
    ts_object_type_members_optional_superset(old_ty, old_source, new_ty, new_source)
}

/// 引数の type annotation が inline object type literal (`{ ... }`) ならそのノードを返す。
fn ts_param_object_type(param: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    let annotation = param.child_by_field_name("type")?;
    let mut cursor = annotation.walk();
    let ty = annotation.named_children(&mut cursor).next()?;
    (ty.kind() == "object_type").then_some(ty)
}

/// old object_type の全メンバーが new にテキスト一致で残り、new の追加メンバーがすべて
/// optional (`?`) の property_signature なら true。メンバー削除・変更は false。
fn ts_object_type_members_optional_superset(
    old_ty: tree_sitter::Node<'_>,
    old_source: &[u8],
    new_ty: tree_sitter::Node<'_>,
    new_source: &[u8],
) -> bool {
    use std::collections::HashMap;
    // normalized text -> 出現ノード列 (同一テキストの重複は TS エラーだが multiset で保守)
    let mut new_members: HashMap<String, Vec<tree_sitter::Node>> = HashMap::new();
    let mut cursor = new_ty.walk();
    for member in new_ty.named_children(&mut cursor) {
        let Some(text) = node_normalized_text(member, new_source) else {
            return false;
        };
        new_members.entry(text).or_default().push(member);
    }
    let mut cursor = old_ty.walk();
    for member in old_ty.named_children(&mut cursor) {
        let Some(text) = node_normalized_text(member, old_source) else {
            return false;
        };
        // 既存メンバーは new 側で 1 つ消費できなければ削除/変更あり → 不成立
        match new_members.get_mut(&text) {
            Some(nodes) if !nodes.is_empty() => {
                nodes.pop();
            }
            _ => return false,
        }
    }
    // 残った new 側メンバー = 追加分。すべて optional property_signature であること。
    new_members.values().flatten().all(|member| {
        member.kind() == "property_signature" && ts_property_signature_is_optional(*member)
    })
}

/// property_signature が optional (`?` トークン付き) か。`?` は named child に現れない
/// anonymous token のため全 child を走査する。
fn ts_property_signature_is_optional(prop: tree_sitter::Node<'_>) -> bool {
    let mut cursor = prop.walk();
    prop.children(&mut cursor).any(|c| c.kind() == "?")
}

/// `Class.method` 形式の 2 要素 qualname を分解する。3 要素以上 (nested class 等) は
/// 解決対象外として None。
fn split_two_segment_qualname(name: &str) -> Option<(&str, &str)> {
    let (class_name, method_name) = name.split_once('.')?;
    if class_name.is_empty() || method_name.is_empty() || method_name.contains('.') {
        return None;
    }
    Some((class_name, method_name))
}

/// トップレベル (または export 文直下) の class 宣言 `class_name` を一意に解決し、
/// その body 直下の method `method_name` を返す。
///
/// blocking 維持 (None) にするケース:
/// - 同名 class がトップレベルに複数ある (どちらの変更か特定できない)
/// - class body 直下に同名 member が複数ある (overload signature / abstract signature /
///   同名 field 等。単一 `method_definition` に絞れない)
/// - 唯一の同名 member が method_definition でない (arrow function field 等)
///
/// nested class / class expression は走査対象にしない (トップレベル children のみ)。
/// computed name (`["x"]`) は name テキストが一致しないため自然に不成立。
fn find_unique_top_level_class_method<'a>(
    root: tree_sitter::Node<'a>,
    source: &[u8],
    class_name: &str,
    method_name: &str,
) -> Option<tree_sitter::Node<'a>> {
    let class_kinds = |k: &str| matches!(k, "class_declaration" | "abstract_class_declaration");
    let mut found_class: Option<tree_sitter::Node<'a>> = None;
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        let candidate = if child.kind() == "export_statement" {
            let mut sub_cursor = child.walk();
            child
                .children(&mut sub_cursor)
                .find(|c| class_kinds(c.kind()))
        } else if class_kinds(child.kind()) {
            Some(child)
        } else {
            None
        };
        if let Some(class_node) = candidate
            && let Some(name_node) = class_node.child_by_field_name("name")
            && let Some(bytes) = source.get(name_node.start_byte()..name_node.end_byte())
            && let Ok(decl_name) = std::str::from_utf8(bytes)
            && decl_name == class_name
        {
            if found_class.is_some() {
                // 同名 class が複数 → 曖昧なので不成立
                return None;
            }
            found_class = Some(class_node);
        }
    }
    let body = found_class?.child_by_field_name("body")?;

    let mut found_method: Option<tree_sitter::Node<'a>> = None;
    let mut matches = 0usize;
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor) {
        let Some(name_node) = member.child_by_field_name("name") else {
            continue;
        };
        let Some(bytes) = source.get(name_node.start_byte()..name_node.end_byte()) else {
            continue;
        };
        let Ok(member_name) = std::str::from_utf8(bytes) else {
            continue;
        };
        if member_name != method_name {
            continue;
        }
        matches += 1;
        if member.kind() == "method_definition" {
            found_method = Some(member);
        }
    }
    // overload signature / abstract signature / 同名 field が併存する場合は単一の
    // method_definition へ安全に対応付けられないため不成立。
    if matches != 1 {
        return None;
    }
    found_method
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TsFunctionParam {
    normalized: String,
    omittable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TsFunctionSignatureParts {
    head: String,
    tail: String,
    params: Vec<TsFunctionParam>,
}

/// function node を head / parameters / tail に分ける。tail には戻り値型や `async` 後続など、
/// parameters 以降から body 直前までを含める。
pub(crate) fn ts_function_signature_parts(
    fn_node: tree_sitter::Node<'_>,
    source: &[u8],
) -> Option<TsFunctionSignatureParts> {
    let params = fn_node.child_by_field_name("parameters")?;
    let sig_start = fn_node.start_byte();
    let sig_end = fn_node
        .child_by_field_name("body")
        .map(|b| b.start_byte())
        .unwrap_or_else(|| fn_node.end_byte());
    let head = normalize_signature_whitespace(source.get(sig_start..params.start_byte())?);
    let tail = normalize_signature_whitespace(source.get(params.end_byte()..sig_end)?);
    let params = ts_function_params(params, source)?;
    Some(TsFunctionSignatureParts { head, tail, params })
}

/// formal_parameters 直下の実引数ノードを抽出する。判定不能な parameter kind が混ざる場合は
/// None にして blocking を維持する。
pub(crate) fn ts_function_params(
    params: tree_sitter::Node<'_>,
    source: &[u8],
) -> Option<Vec<TsFunctionParam>> {
    let mut result = Vec::new();
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        match child.kind() {
            "required_parameter" | "optional_parameter" | "formal_parameter" | "identifier" => {
                let text = source.get(child.start_byte()..child.end_byte())?;
                result.push(TsFunctionParam {
                    normalized: normalize_signature_whitespace(text),
                    omittable: ts_param_is_omittable(child),
                });
            }
            // rest parameter の追加は呼び出し側 arity 互換ではあっても型契約の意図を
            // ここでは保証しないため、互換降格しない。
            "rest_pattern" => return None,
            _ => return None,
        }
    }
    Some(result)
}

/// 引数が呼び出し側から省略可能かを AST 上で判定する。
pub(crate) fn ts_param_is_omittable(param: tree_sitter::Node<'_>) -> bool {
    param.kind() == "optional_parameter" || param.child_by_field_name("value").is_some()
}

/// old の全引数が new の先頭と一致し、new の追加分がすべて省略可能なら true。
pub(crate) fn ts_params_prefix_same_with_optional_tail(
    old_params: &[TsFunctionParam],
    new_params: &[TsFunctionParam],
) -> bool {
    if new_params.len() <= old_params.len() {
        return false;
    }
    for (old, new) in old_params.iter().zip(new_params.iter()) {
        if old != new {
            return false;
        }
    }
    new_params[old_params.len()..].iter().all(|p| p.omittable)
}

#[derive(Debug, Clone)]
pub(crate) struct ObjectMemberKeys {
    pub(crate) member_keys: HashSet<String>,
    pub(crate) record_keys: Option<HashSet<String>>,
    /// flat object のとき: top-level key → 値の正規化テキスト (空白を 1 個に畳んだもの)。
    /// record のとき: (record key, member key) → nested 値の正規化テキスト。
    ///
    /// キー集合だけでは「既存キーの値が別物に差し替わった」破壊的変更
    /// (`{a: () => {}}` → `{a: 42, b: () => {}}`) を検出できないため、
    /// 両側に残存するキーの値同一性を照合する用途で持つ。
    pub(crate) entry_values: HashMap<ObjectEntryKey, String>,
    /// `variable_declarator` の型注釈 (`const cfg: { alpha: number } = ...`) の token 列。
    /// 注釈が無ければ `None`。
    ///
    /// **型注釈を見ずに member キーだけで降格してはいけない**: `cfg: { alpha: number }` →
    /// `cfg: { alpha: number | string; beta: number }` は「キー追加」だけを見ると後方互換に
    /// 見えるが、公開されているのは注釈の型なので利用側は `TS2322` で落ちる。
    pub(crate) declarator_type: Option<LeafTokens>,
    /// object literal を包む `as` / `satisfies` の連なり。要素は
    /// `(ノード種別, 型部分の token 列)`。冗長括弧は整形なので記録しない。
    ///
    /// `as const` の付け外しや `satisfies T` の型変更は公開契約の変更なので、
    /// 無条件に剥がして member キーだけ比較すると誤って降格する。
    pub(crate) wrappers: Vec<ObjectLiteralWrapper>,
    /// homogeneous record のとき: record key → その entry (nested object) を包む
    /// `as` / `satisfies` の連なり。
    ///
    /// トップレベルの `wrappers` だけでは entry 側の wrapper 変更を見逃す。
    /// `{ google: { alpha: 1 } as const }` → `{ google: { alpha: 1, beta: 2 } }` は
    /// トップレベルの wrapper も型注釈も不変で、共通キーの値も同じなので「キー追加のみ」に
    /// 見えるが、公開型は `{ readonly alpha: 1 }` から `{ alpha: number; beta: number }` へ
    /// 変わる。
    pub(crate) entry_wrappers: HashMap<String, Vec<ObjectLiteralWrapper>>,
}

/// `entry_values` のキー。flat は top-level key 単独、record は (record key, member key)。
pub(crate) type ObjectEntryKey = (String, Option<String>);

/// 値ノードのソーステキストを、空白の連なりを 1 個の空白に畳んで正規化する。
/// 整形 (prettier 等) の差分だけで破壊的変更と誤判定しないための最小限の正規化。
fn normalized_value_text(node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    let raw = node.utf8_text(source).ok()?;
    let mut out = String::with_capacity(raw.len());
    let mut in_ws = false;
    for ch in raw.chars() {
        if ch.is_whitespace() {
            in_ws = true;
            continue;
        }
        if in_ws && !out.is_empty() {
            out.push(' ');
        }
        in_ws = false;
        out.push(ch);
    }
    Some(out)
}

/// TS/TSX/JS ソースから、トップレベル exported な `name` の初期化子 object literal の
/// member schema を抽出する。
///
/// - flat object: top-level key を `member_keys` とする
/// - homogeneous record: top-level key を `record_keys`、各 value object の共通 key を
///   `member_keys` とする
///
/// `as const` / `satisfies T` は unwrap する。object literal でない / spread / computed key /
/// mixed shape / record schema 不揃い / 宣言が見つからない / 同名複数なら None (呼び出し側で
/// blocking 維持)。
pub(crate) fn extract_object_member_keys(
    source: &[u8],
    lang_id: crate::language::LangId,
    name: &str,
) -> Option<ObjectMemberKeys> {
    let tree = parser::parse_source(source, lang_id).ok()?;
    let root = tree.root_node();
    let decls = find_toplevel_decls_named(root, name, source);
    if decls.len() != 1 {
        return None;
    }
    let value = decls[0].child_by_field_name("value")?;
    let (obj, wrappers) = unwrap_to_object_literal_with_wrappers(value, source)?;
    // 型注釈は object literal の外にある公開契約なので、member キーとは別に持って比較する。
    let declarator_type = match decls[0].child_by_field_name("type") {
        Some(annotation) => {
            let mut tokens = Vec::new();
            collect_leaf_tokens(annotation, source, &mut tokens)?;
            Some(tokens)
        }
        None => None,
    };
    let mut keys = collect_object_keys(obj, source)?;
    keys.declarator_type = declarator_type;
    keys.wrappers = wrappers;
    Some(keys)
}

/// AST の leaf token 列。要素は `(ノード種別, 元テキスト)`。
pub(crate) type LeafTokens = Vec<(String, String)>;

/// object literal を包む `as` / `satisfies` の 1 段。`(ノード種別, 型部分の token 列)`。
pub(crate) type ObjectLiteralWrapper = (String, LeafTokens);

/// `unwrap_to_object_literal` の wrapper 記録版。剥がした `as` / `satisfies` を
/// `(ノード種別, 型部分の token 列)` として順に返す。冗長括弧は整形なので記録しない。
///
/// 型部分は「wrapper 全体の leaf token 列」から「内側の式の leaf token 列」を除いた残り。
/// leaf の pre-order 走査では内側の式 (`named_child(0)` = 最左) のトークンが先に並ぶため、
/// 先頭ぶんを落とせば `as const` / `satisfies Shape` の部分がそのまま残る。
fn unwrap_to_object_literal_with_wrappers<'tree>(
    node: tree_sitter::Node<'tree>,
    source: &[u8],
) -> Option<(tree_sitter::Node<'tree>, Vec<ObjectLiteralWrapper>)> {
    let mut cur = node;
    let mut wrappers = Vec::new();
    loop {
        match cur.kind() {
            "object" => return Some((cur, wrappers)),
            "parenthesized_expression" => {
                cur = cur.named_child(0)?;
            }
            "as_expression" | "satisfies_expression" => {
                let inner = cur.named_child(0)?;
                let mut whole = Vec::new();
                collect_leaf_tokens(cur, source, &mut whole)?;
                let mut inner_tokens = Vec::new();
                collect_leaf_tokens(inner, source, &mut inner_tokens)?;
                // 長さだけでなく prefix 一致も確認する。grammar 変更や異常 AST で
                // 内側の式が最左でなくなった場合に、無関係な token を型部分として
                // 拾わず fail-closed に倒れる。
                if !whole.starts_with(&inner_tokens) {
                    return None;
                }
                wrappers.push((cur.kind().to_string(), whole[inner_tokens.len()..].to_vec()));
                cur = inner;
            }
            _ => return None,
        }
    }
}

/// `expr as const` / `expr satisfies T` / 冗長括弧 `(expr)` をはがして object literal
/// ノードを返す。
pub(crate) fn unwrap_to_object_literal(node: tree_sitter::Node) -> Option<tree_sitter::Node> {
    let mut cur = node;
    loop {
        match cur.kind() {
            "object" => return Some(cur),
            // parenthesized_expression: `({ a: 1 })` の冗長括弧。as/satisfies と同じく
            // named_child(0) が内側の式なので 1 段剥がす (`({ a: 1 } as const)` も透過)。
            "as_expression" | "satisfies_expression" | "parenthesized_expression" => {
                cur = cur.named_child(0)?;
            }
            _ => return None,
        }
    }
}

/// object literal の shape を flat / homogeneous record に分類して property キーを集める。
/// mixed shape / record schema 不揃い / spread (`...x`) / computed key (`[expr]:`) があれば None。
pub(crate) fn collect_object_keys(
    obj: tree_sitter::Node,
    source: &[u8],
) -> Option<ObjectMemberKeys> {
    let mut top_level_keys = HashSet::new();
    let mut entry_values: HashMap<ObjectEntryKey, String> = HashMap::new();
    let mut record_member_keys: Option<HashSet<String>> = None;
    let mut entry_wrappers: HashMap<String, Vec<ObjectLiteralWrapper>> = HashMap::new();
    let mut has_object_value = false;
    let mut has_non_object_value = false;
    let mut cursor = obj.walk();
    for child in obj.named_children(&mut cursor) {
        match child.kind() {
            "pair" => {
                let key = child.child_by_field_name("key")?;
                let key_text = object_key_text(key, source)?;
                top_level_keys.insert(key_text.clone());
                let value = child.child_by_field_name("value")?;
                if let Some((nested, nested_wrappers)) =
                    unwrap_to_object_literal_with_wrappers(value, source)
                {
                    has_object_value = true;
                    entry_wrappers.insert(key_text.clone(), nested_wrappers);
                    let nested_keys = collect_flat_object_keys(nested, source)?;
                    // record は nested の member 単位で値を持つ。entry 全体をまとめて
                    // 比較すると member キーの追加/削除だけで差分になり、本判定の
                    // 目的 (未参照 member キーの削除を降格する) が成立しなくなる。
                    collect_flat_object_values(nested, source, &key_text, &mut entry_values)?;
                    match &record_member_keys {
                        Some(existing) if existing != &nested_keys => return None,
                        Some(_) => {}
                        None => record_member_keys = Some(nested_keys),
                    }
                } else {
                    has_non_object_value = true;
                    entry_values.insert((key_text, None), normalized_value_text(value, source)?);
                }
            }
            "shorthand_property_identifier" => {
                // `{ foo }` は値が識別子そのもの。キー名と同一テキストになる。
                let key_text = child.utf8_text(source).ok()?.to_string();
                top_level_keys.insert(key_text.clone());
                entry_values.insert((key_text.clone(), None), key_text);
                has_non_object_value = true;
            }
            // spread は shape を静的確定できないので blocking
            "spread_element" => return None,
            _ => {}
        }
    }
    if has_object_value && has_non_object_value {
        return None;
    }
    if has_object_value {
        return Some(ObjectMemberKeys {
            member_keys: record_member_keys?,
            record_keys: Some(top_level_keys),
            entry_values,
            // 宣言側の情報は object literal から見えないので、呼び出し元
            // (`extract_object_member_keys`) が埋める。
            declarator_type: None,
            wrappers: Vec::new(),
            entry_wrappers,
        });
    }
    Some(ObjectMemberKeys {
        member_keys: top_level_keys,
        record_keys: None,
        entry_values,
        declarator_type: None,
        wrappers: Vec::new(),
        entry_wrappers,
    })
}

/// record の 1 entry (nested object) から (record key, member key) → 値テキストを集める。
/// nested object は再帰しない (`collect_flat_object_keys` と同じ 1 階層のみ)。
fn collect_flat_object_values(
    obj: tree_sitter::Node,
    source: &[u8],
    record_key: &str,
    out: &mut HashMap<ObjectEntryKey, String>,
) -> Option<()> {
    let mut cursor = obj.walk();
    for child in obj.named_children(&mut cursor) {
        match child.kind() {
            "pair" => {
                let key = child.child_by_field_name("key")?;
                let member = object_key_text(key, source)?;
                let value = child.child_by_field_name("value")?;
                out.insert(
                    (record_key.to_string(), Some(member)),
                    normalized_value_text(value, source)?,
                );
            }
            "shorthand_property_identifier" => {
                let member = child.utf8_text(source).ok()?.to_string();
                out.insert((record_key.to_string(), Some(member.clone())), member);
            }
            "spread_element" => return None,
            _ => {}
        }
    }
    Some(())
}

/// flat object として 1 階層分の property キーだけを抽出する。nested object を再帰しない。
pub(crate) fn collect_flat_object_keys(
    obj: tree_sitter::Node,
    source: &[u8],
) -> Option<HashSet<String>> {
    let mut keys = HashSet::new();
    let mut cursor = obj.walk();
    for child in obj.named_children(&mut cursor) {
        match child.kind() {
            "pair" => {
                let key = child.child_by_field_name("key")?;
                keys.insert(object_key_text(key, source)?);
            }
            "shorthand_property_identifier" => {
                keys.insert(child.utf8_text(source).ok()?.to_string());
            }
            "spread_element" => return None,
            _ => {}
        }
    }
    Some(keys)
}

pub(crate) fn object_key_text(key: tree_sitter::Node, source: &[u8]) -> Option<String> {
    match key.kind() {
        "property_identifier" | "shorthand_property_identifier" => {
            Some(key.utf8_text(source).ok()?.to_string())
        }
        "string" => Some(static_js_string_text(key, source)?.to_string()),
        // computed key は静的解析できないので blocking
        _ => None,
    }
}

/// `keys` のいずれかへの member access (`.key` / `['key']` / `["key"]`) が repo 全体に
/// 残っているか。解析失敗は保守的に true (残存ありとみなし blocking)。
fn member_access_keys_have_ref(dir: &str, keys: &HashSet<&str>) -> bool {
    if keys.is_empty() {
        return false;
    }
    let ac = match aho_corasick::AhoCorasick::new(keys.iter().copied()) {
        Ok(ac) => ac,
        Err(_) => return true,
    };
    let files = match crate::engine::refs::collect_files(std::path::Path::new(dir), None) {
        Ok(files) => files,
        Err(_) => return true,
    };
    files
        .into_par_iter()
        .any(|path| file_has_any_member_access_ref(path.as_path(), keys, &ac).unwrap_or(true))
}

fn file_has_any_member_access_ref(
    path: &std::path::Path,
    keys: &HashSet<&str>,
    ac: &aho_corasick::AhoCorasick,
) -> Result<bool> {
    use crate::language::LangId;
    let Some(path_str) = path.to_str() else {
        return Ok(true);
    };
    let utf8_path = camino::Utf8Path::new(path_str);
    let lang = match LangId::from_path(utf8_path) {
        Ok(lang @ (LangId::Javascript | LangId::Typescript | LangId::Tsx)) => lang,
        Err(_) if path.extension().is_none() => {
            let source = parser::read_file(utf8_path)?;
            return match LangId::detect(utf8_path, source.as_bytes()) {
                Ok(lang @ (LangId::Javascript | LangId::Typescript | LangId::Tsx)) => {
                    source_has_any_member_access_ref_with_ac(source.as_bytes(), lang, keys, ac)
                }
                Ok(_) | Err(_) => Ok(false),
            };
        }
        Ok(_) | Err(_) => return Ok(false),
    };
    let source = parser::read_file(utf8_path)?;
    source_has_any_member_access_ref_with_ac(source.as_bytes(), lang, keys, ac)
}

#[cfg(test)]
pub(crate) fn source_has_member_access_ref(
    source: &[u8],
    lang: crate::language::LangId,
    key: &str,
) -> Result<bool> {
    if memchr::memmem::find(source, key.as_bytes()).is_none() {
        return Ok(false);
    }
    let tree = parser::parse_source(source, lang)?;
    Ok(ast_has_member_access_ref(tree.root_node(), source, key))
}

/// 複数キー版の source 判定。テストから batch 経路の意味論を直接固定するため公開する。
#[cfg(test)]
pub(crate) fn source_has_any_member_access_ref(
    source: &[u8],
    lang: crate::language::LangId,
    keys: &HashSet<&str>,
) -> Result<bool> {
    if keys.is_empty() {
        return Ok(false);
    }
    let ac = aho_corasick::AhoCorasick::new(keys.iter().copied())?;
    source_has_any_member_access_ref_with_ac(source, lang, keys, &ac)
}

fn source_has_any_member_access_ref_with_ac(
    source: &[u8],
    lang: crate::language::LangId,
    keys: &HashSet<&str>,
    ac: &aho_corasick::AhoCorasick,
) -> Result<bool> {
    if !ac.is_match(source) {
        return Ok(false);
    }
    let tree = parser::parse_source(source, lang)?;
    Ok(ast_has_any_member_access_ref(
        tree.root_node(),
        source,
        keys,
    ))
}

#[cfg(test)]
pub(crate) fn ast_has_member_access_ref(node: tree_sitter::Node, source: &[u8], key: &str) -> bool {
    if node_is_member_access_ref(node, source, key) {
        return true;
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .any(|child| ast_has_member_access_ref(child, source, key))
}

fn ast_has_any_member_access_ref(
    node: tree_sitter::Node,
    source: &[u8],
    keys: &HashSet<&str>,
) -> bool {
    if member_access_ref_key(node, source).is_some_and(|key| keys.contains(key)) {
        return true;
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .any(|child| ast_has_any_member_access_ref(child, source, keys))
}

#[cfg(test)]
pub(crate) fn node_is_member_access_ref(node: tree_sitter::Node, source: &[u8], key: &str) -> bool {
    member_access_ref_key(node, source) == Some(key)
}

fn member_access_ref_key<'a>(node: tree_sitter::Node, source: &'a [u8]) -> Option<&'a str> {
    match node.kind() {
        "member_expression" => node
            .child_by_field_name("property")
            .and_then(|property| property.utf8_text(source).ok()),
        "subscript_expression" => node
            .child_by_field_name("index")
            .filter(|index| index.kind() == "string")
            .and_then(|index| static_js_string_text(index, source)),
        // destructuring (`const { beta } = config;`) も member の実利用。
        // shorthand (`{ beta }` / `{ beta = 1 }` の左辺) は
        // shorthand_property_identifier_pattern、rename (`{ beta: b }`) は pair_pattern の
        // key に現れる。見落とすと破壊的な member 削除が unused_object_members に降格する。
        "shorthand_property_identifier_pattern" => node.utf8_text(source).ok(),
        "pair_pattern" => {
            let key_node = node.child_by_field_name("key")?;
            match key_node.kind() {
                "string" => static_js_string_text(key_node, source),
                _ => key_node.utf8_text(source).ok(),
            }
        }
        _ => None,
    }
}

pub(crate) fn static_js_string_text<'a>(
    node: tree_sitter::Node,
    source: &'a [u8],
) -> Option<&'a str> {
    let raw = node.utf8_text(source).ok()?;
    let bytes = raw.as_bytes();
    if bytes.len() < 2 {
        return None;
    }
    let quote = bytes[0];
    let end = *bytes.last()?;
    if matches!(quote, b'\'' | b'"' | b'`') && quote == end {
        Some(&raw[1..raw.len() - 1])
    } else {
        None
    }
}

/// 関数 parameters が「単一の destructured object parameter で、呼び出し側から
/// 引数省略可能 (`foo()` で valid) と判定できる」場合に true。
///
/// 判定基準:
/// - parameters の named child が 1 個 (required_parameter / optional_parameter)
/// - その pattern が object_pattern
/// - 以下のいずれかを満たす:
///   1. parameter に default value (`= {}` 等の initializer) がある
///   2. type annotation の型が「全 optional な object type」と証明できる
///      - inline `object_type` ですべての property が `?` 付き (空も含む)
///      - 同一ファイル内の `interface` / `type alias` で同名のものが見つかり、
///        その body / value が全 optional な object type
///
/// import 型 / generic / intersection / conditional type は False を返す (型推論が
/// 必要なため、AST だけでは省略可能性を保証できない。codex 設計合意)。
pub(crate) fn is_optionally_omittable_single_destructured_param(
    params: tree_sitter::Node<'_>,
    root: tree_sitter::Node<'_>,
    source: &[u8],
) -> bool {
    let mut cursor = params.walk();
    let param_nodes: Vec<tree_sitter::Node<'_>> = params
        .children(&mut cursor)
        .filter(|n| matches!(n.kind(), "required_parameter" | "optional_parameter"))
        .collect();
    if param_nodes.len() != 1 {
        return false;
    }
    let param = param_nodes[0];

    // pattern が object_pattern
    let Some(pattern) = param.child_by_field_name("pattern") else {
        return false;
    };
    if pattern.kind() != "object_pattern" {
        return false;
    }

    // 1. default value (`= {}` 等の initializer) があるなら無条件で省略可能
    if param.child_by_field_name("value").is_some() {
        return true;
    }

    // 2. type annotation を取得 (`: T` の T を取り出す)
    let Some(type_annot) = param.child_by_field_name("type") else {
        return false;
    };
    // type_annotation の named child の最後が型ノード
    let mut tc = type_annot.walk();
    let type_node = type_annot.named_children(&mut tc).last();
    let Some(type_node) = type_node else {
        return false;
    };

    if type_node.kind() == "object_type" {
        return all_object_type_members_optional(type_node, source);
    }
    if type_node.kind() == "type_identifier" {
        let Some(name_bytes) = source.get(type_node.start_byte()..type_node.end_byte()) else {
            return false;
        };
        let Ok(name) = std::str::from_utf8(name_bytes) else {
            return false;
        };
        let decls = collect_top_level_type_decls(root, source, name);
        return !decls.is_empty()
            && decls
                .iter()
                .all(|d| single_type_decl_all_optional(*d, source));
    }
    false
}

/// `object_type` (TS の inline `{ x?: T; y: U }`) のすべての property が `?` 付き
/// optional ならば true。method_signature / index_signature がある場合は false
/// (これらは optional マーカーの一般判定が複雑になるため保守的に拒否)。
/// property が 1 つもない (空 `{}`) ケースも全 optional と同等扱いで true。
pub(crate) fn all_object_type_members_optional(
    object_type: tree_sitter::Node<'_>,
    source: &[u8],
) -> bool {
    let mut cursor = object_type.walk();
    for child in object_type.children(&mut cursor) {
        match child.kind() {
            "property_signature" if !property_signature_has_optional_marker(child, source) => {
                return false;
            }
            "method_signature" | "index_signature" | "construct_signature" | "call_signature" => {
                return false;
            }
            _ => {}
        }
    }
    true
}

/// `property_signature` ノードに optional マーカー `?` が付いているかを tree-sitter
/// の `?` token を直接見て判定する。`"name?": string` のような string property
/// の名前に `?` を含むケースは誤判定しない。
pub(crate) fn property_signature_has_optional_marker(
    prop: tree_sitter::Node<'_>,
    _source: &[u8],
) -> bool {
    let mut cursor = prop.walk();
    for child in prop.children(&mut cursor) {
        match child.kind() {
            "?" => return true,
            "type_annotation" => return false,
            _ => {}
        }
    }
    false
}

/// `root` のトップレベル (program 直下 / `export_statement` 直下) にある
/// `interface_declaration` / `type_alias_declaration` のうち、name フィールドが
/// 指定名と一致するものを **すべて** 集める。interface declaration merge 対応の
/// ために複数返す。
///
/// ネストした declaration (関数内 / ブロック内) や import 型の解決はしない。
/// 関数 scope などローカル scope の declaration を誤って拾わないため、スコープを
/// トップレベルに限定する (codex 指摘 3 対応)。
pub(crate) fn collect_top_level_type_decls<'a>(
    root: tree_sitter::Node<'a>,
    source: &[u8],
    name: &str,
) -> Vec<tree_sitter::Node<'a>> {
    let mut decls = Vec::new();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        let candidate = if child.kind() == "export_statement" {
            let mut sub_cursor = child.walk();
            child
                .children(&mut sub_cursor)
                .find(|c| matches!(c.kind(), "interface_declaration" | "type_alias_declaration"))
        } else if matches!(
            child.kind(),
            "interface_declaration" | "type_alias_declaration"
        ) {
            Some(child)
        } else {
            None
        };
        if let Some(decl) = candidate
            && let Some(name_node) = decl.child_by_field_name("name")
            && let Some(bytes) = source.get(name_node.start_byte()..name_node.end_byte())
            && let Ok(decl_name) = std::str::from_utf8(bytes)
            && decl_name == name
        {
            decls.push(decl);
        }
    }
    decls
}

/// 単一の `interface_declaration` / `type_alias_declaration` のメンバが全 optional な
/// object 型かを判定する。
///
/// - `interface_declaration` が `extends_type_clause` を持つ場合は base interface が
///   required field を持つ可能性があるため保守的に false (codex 指摘 2 対応)
/// - `type_alias_declaration` は value が `object_type` のケースのみ判定対象。
///   union / intersection / generic / conditional / mapped 等は保守的に false
pub(crate) fn single_type_decl_all_optional(decl: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    match decl.kind() {
        "interface_declaration" => {
            if interface_has_extends(decl) {
                return false;
            }
            if let Some(body) = decl.child_by_field_name("body") {
                return all_object_type_members_optional(body, source);
            }
            false
        }
        "type_alias_declaration" => {
            if let Some(value) = decl.child_by_field_name("value")
                && value.kind() == "object_type"
            {
                return all_object_type_members_optional(value, source);
            }
            false
        }
        _ => false,
    }
}

/// `interface_declaration` ノードが `extends_type_clause` を持つかを判定する。
pub(crate) fn interface_has_extends(decl: tree_sitter::Node<'_>) -> bool {
    let mut cursor = decl.walk();
    decl.children(&mut cursor)
        .any(|c| c.kind() == "extends_type_clause")
}

/// TS/TSX 関数の「引数なし `()` から省略可能 destructured 引数追加」が
/// backward-compatible かを判定する。両側 signature を見て判定するため
/// `detect_api_changes` から呼ぶ。`extract_api_signature` で signature 単独
/// 正規化に組み込まないのは、optional 型変更 (`{x?:string}` → `{x?:number}`)
/// まで誤って互換扱いするのを防ぐため (codex 設計合意)。
///
/// 条件:
/// 1. `new_path` の言語が TypeScript / Tsx
/// 2. `new_sig` に `fn_name({}` (destructure normalize 済み) が含まれる
///    (早期 reject 用の文字列マッチ)
/// 3. 旧ツリー (`base:old_path`) のトップレベル関数 `fn_name` の parameters が
///    **AST 上で** 空 (codex 指摘: 文字列 contains だと型注釈内 call signature
///    `{ fn_name(): void }` を誤検出するため、必ず AST で確認する)
/// 4. 新ツリー (`new_path`) のトップレベル関数 `fn_name` の parameters が省略
///    可能と判定できる
///
/// `old_path` と `new_path` は rename 差分に対応するため別々に渡す。
pub(crate) fn is_ts_no_arg_to_optional_destructured_compatible(
    old_sig: &str,
    new_sig: &str,
    dir: &str,
    base: &str,
    old_path: &str,
    new_path: &str,
    fn_name: &str,
) -> bool {
    let full_new_path = std::path::Path::new(dir).join(new_path);
    let Some(utf8_str) = full_new_path.to_str() else {
        return false;
    };
    let utf8_new_path = camino::Utf8Path::new(utf8_str);
    let Ok(lang_id) = crate::language::LangId::from_path(utf8_new_path) else {
        return false;
    };
    if !matches!(
        lang_id,
        crate::language::LangId::Typescript | crate::language::LangId::Tsx
    ) {
        return false;
    }

    // 早期 reject (高速化): 新 sig が destructure 形式でなければ判定不要
    if !signature_has_destructured_params_for(new_sig, fn_name) {
        return false;
    }
    // 早期 reject (高速化): 旧 sig 文字列に `fn_name()` パターンがなければ判定不要。
    // 文字列 contains は false-positive あり (型注釈内 call signature) のため、これは
    // 単なる早期スクリーニング。確実な判定は次の AST 検査で行う。
    if !signature_has_empty_parens_for(old_sig, fn_name) {
        return false;
    }
    // 旧ツリーで AST 検査: トップレベル関数 fn_name の parameters が実際に空か。
    // rename 差分では `df.old_path` を使うため、`old_path` を渡す。
    if !old_top_level_function_has_empty_parameters(dir, base, old_path, lang_id, fn_name) {
        return false;
    }

    let Ok(source) = parser::read_file(utf8_new_path) else {
        return false;
    };
    let Ok(tree) = parser::parse_source(&source, lang_id) else {
        return false;
    };
    let root = tree.root_node();

    let Some(fn_node) = find_top_level_function_by_name(root, &source, fn_name) else {
        return false;
    };
    let Some(params) = fn_node.child_by_field_name("parameters") else {
        return false;
    };
    is_optionally_omittable_single_destructured_param(params, root, &source)
}

/// signature 文字列に `fn_name()` (parameters なし) パターンが含まれるかを判定。
/// 注: これは早期 reject 用のスクリーニング。型注釈内の call signature を誤検出する
/// 可能性があるため、確実な判定には AST 検査 (`old_top_level_function_has_empty_parameters`)
/// を併用する。
pub(crate) fn signature_has_empty_parens_for(sig: &str, fn_name: &str) -> bool {
    let needle = format!("{fn_name}()");
    sig.contains(&needle)
}

/// signature 文字列に destructure normalize 済みの `fn_name({}` パターンが
/// 含まれるかを判定。
pub(crate) fn signature_has_destructured_params_for(sig: &str, fn_name: &str) -> bool {
    let needle = format!("{fn_name}({{}}");
    sig.contains(&needle)
}

/// 旧ツリー (base リビジョン) を `git show` で取得して parse し、トップレベル関数
/// `fn_name` の parameters が空かを AST で判定する。
///
/// signature 文字列の `fn_name()` パターン検査だけでは型注釈内 call signature を
/// 誤検出するため、最終確認として AST 検査が必要。
///
/// `base` / `file_path` の検証は `git_show_blob` 側で強制される (codex 指摘: 既存の
/// `extract_exported_symbols_from_git` と同じ防御を行わないと `--diff` / stdin 経路で
/// 未検証の `base` がここに到達し得る)。
pub(crate) fn old_top_level_function_has_empty_parameters(
    dir: &str,
    base: &str,
    file_path: &str,
    lang_id: crate::language::LangId,
    fn_name: &str,
) -> bool {
    let Some(source) = git_show_blob(dir, base, file_path) else {
        return false;
    };
    let Ok(tree) = parser::parse_source(&source, lang_id) else {
        return false;
    };
    let Some(fn_node) = find_top_level_function_by_name(tree.root_node(), &source, fn_name) else {
        return false;
    };
    let Some(params) = fn_node.child_by_field_name("parameters") else {
        return false;
    };
    let mut cursor = params.walk();
    params.named_children(&mut cursor).count() == 0
}

/// `root` のトップレベル (program 直下 / `export_statement` 直下) にある関数 /
/// メソッド宣言のうち、name が一致するものを返す。ネストしたローカル関数や
/// 関数式内の同名宣言は対象外 (codex 指摘 6 対応)。
pub(crate) fn find_top_level_function_by_name<'a>(
    root: tree_sitter::Node<'a>,
    source: &[u8],
    name: &str,
) -> Option<tree_sitter::Node<'a>> {
    let fn_kinds = |k: &str| {
        matches!(
            k,
            "function_declaration"
                | "function_definition"
                | "method_definition"
                | "function_signature_item"
        )
    };
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        let candidate = if child.kind() == "export_statement" {
            let mut sub_cursor = child.walk();
            child.children(&mut sub_cursor).find(|c| fn_kinds(c.kind()))
        } else if fn_kinds(child.kind()) {
            Some(child)
        } else {
            None
        };
        if let Some(fn_node) = candidate
            && let Some(name_node) = fn_node.child_by_field_name("name")
            && let Some(bytes) = source.get(name_node.start_byte()..name_node.end_byte())
            && let Ok(decl_name) = std::str::from_utf8(bytes)
            && decl_name == name
        {
            return Some(fn_node);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::LangId;

    fn keys(src: &str, name: &str) -> Option<ObjectMemberKeys> {
        extract_object_member_keys(src.as_bytes(), LangId::Typescript, name)
    }

    /// 冗長括弧付き object literal `({ ... })` も unwrap して member schema を取れる (B-4)。
    /// 透過しないと unused object member 削除の互換降格が効かず blocking のまま残っていた。
    #[test]
    fn parenthesized_object_literal_is_unwrapped() {
        let src = "export const config = ({ a: 1, b: 2 });\n";
        let r = keys(src, "config").expect("括弧付き object literal を抽出できるべき");
        let expected: HashSet<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        assert_eq!(r.member_keys, expected);
        assert!(r.record_keys.is_none());
    }

    /// 括弧 + `as const` の組み合わせ `({ ... } as const)` も透過できる。
    #[test]
    fn parenthesized_with_as_const_is_unwrapped() {
        let src = "export const config = ({ a: 1 } as const);\n";
        let r = keys(src, "config").expect("括弧 + as const を抽出できるべき");
        let expected: HashSet<String> = ["a"].iter().map(|s| s.to_string()).collect();
        assert_eq!(r.member_keys, expected);
    }

    /// 括弧なしの通常 object literal は従来どおり抽出できる (回帰確認)。
    #[test]
    fn plain_object_literal_is_unwrapped() {
        let src = "export const config = { a: 1 };\n";
        let r = keys(src, "config").expect("通常 object literal を抽出できるべき");
        let expected: HashSet<String> = ["a"].iter().map(|s| s.to_string()).collect();
        assert_eq!(r.member_keys, expected);
    }

    fn literal_values(source: &str, name: &str) -> Option<BTreeSet<TsLiteralValue>> {
        let tree =
            parser::parse_source(source.as_bytes(), LangId::Typescript).expect("parse TypeScript");
        assert!(!tree.root_node().has_error(), "fixture must parse");
        eval_named_literal_union(tree.root_node(), source.as_bytes(), name)
    }

    #[test]
    fn literal_union_alias_chain_is_order_and_duplicate_insensitive() {
        let direct = literal_values(r#"type Category = "x" | "y";"#, "Category");
        let aliased = literal_values(
            concat!(
                "type Base = \"y\" | \"x\" | \"x\";\n",
                "type Category = (Base);\n"
            ),
            "Category",
        );
        assert_eq!(direct, aliased);
        assert!(direct.is_some(), "finite literal union must be evaluable");
    }

    #[test]
    fn literal_union_value_change_is_not_equivalent() {
        let old = literal_values(r#"type Category = "x" | "y";"#, "Category");
        let widened = literal_values(r#"type Category = "x" | "y" | "z";"#, "Category");
        assert_ne!(old, widened);
    }

    #[test]
    fn literal_union_import_cycle_and_non_literal_are_not_evaluable() {
        assert!(
            literal_values(
                concat!(
                    "import type { Base } from \"./base\";\n",
                    "type Category = Base;\n"
                ),
                "Category"
            )
            .is_none()
        );
        assert!(
            literal_values(
                concat!("type Base = Category;\n", "type Category = Base;\n"),
                "Category"
            )
            .is_none()
        );
        assert!(literal_values("type Category = string;", "Category").is_none());
    }

    #[test]
    fn literal_union_deep_parentheses_stop_at_depth_limit() {
        let nested = format!("type Category = {}\"x\"{};", "(".repeat(64), ")".repeat(64));
        assert!(literal_values(&nested, "Category").is_none());
    }
}
