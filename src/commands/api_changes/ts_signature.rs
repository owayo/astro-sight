//! TS/TSX/JS 固有のシグネチャ解析と互換 API 変更判定ヘルパー
//! (React HOC ラップ / object member / 末尾 optional 引数 / 引数なし→省略可能 destructured)。

use anyhow::Result;
use rayon::prelude::*;
use std::collections::HashSet;

use crate::engine::parser;
use crate::models::review::CompatibleApiModification;

use super::super::git_input::validate_git_revision;
use super::{ApiRefIndex, has_blocking_value_usage, normalize_signature_whitespace};

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
#[allow(clippy::too_many_arguments)]
pub(crate) fn detect_react_wrapper_compatible_mod(
    index: &ApiRefIndex,
    dir: &str,
    base: &str,
    old_path: &str,
    new_path: &str,
    name: &str,
    kind: &str,
    old_sig: &str,
    new_sig: &str,
    lang_id: Option<crate::language::LangId>,
) -> Option<CompatibleApiModification> {
    use crate::language::LangId;
    let lang =
        lang_id.filter(|l| matches!(l, LangId::Typescript | LangId::Tsx | LangId::Javascript))?;
    // new 側が memo / forwardRef でラップされていること (単なる function 本体変更は対象外)。
    if !new_sig_has_react_wrapper(new_sig) {
        return None;
    }
    // old 側は非 wrapper (function 宣言等) であること。wrapper-to-wrapper の変更
    // (`forwardRef<HTMLDivElement, P>` → `forwardRef<HTMLButtonElement, P>` 等) は ref 型や
    // generic の差分を取りこぼすため対象外 (codex 指摘)。
    if new_sig_has_react_wrapper(old_sig) {
        return None;
    }
    // 信頼境界外のパスは多層防御で再チェックする。
    if !crate::engine::impact::is_safe_diff_path(old_path)
        || !crate::engine::impact::is_safe_diff_path(new_path)
    {
        return None;
    }
    // old は base リビジョン、new は working tree からソースを再取得して props 型を AST 抽出
    // する。signature 文字列は const の先頭行 fallback で複数行 destructured props の型注釈を
    // 取りこぼすため、ソース再パースで比較する (codex 設計合意)。
    validate_git_revision(base, "--base").ok()?;
    validate_git_revision(old_path, "diff file path").ok()?;
    let old_output = std::process::Command::new("git")
        .args(["show", &format!("{base}:{old_path}")])
        .current_dir(dir)
        .output()
        .ok()?;
    if !old_output.status.success() {
        return None;
    }
    let new_full = std::path::Path::new(dir).join(new_path);
    let new_utf8 = camino::Utf8Path::from_path(&new_full)?;
    let new_source = parser::read_file(new_utf8).ok()?;
    // old / new 双方の第1引数 (props) の型注釈を抽出して一致を要求する。
    let old_props = extract_component_props_type(&old_output.stdout, lang, name)?;
    let new_props = extract_component_props_type(&new_source, lang, name)?;
    if old_props != new_props {
        return None;
    }
    // 値利用 (呼び出し / typeof / member / new / indexed) が残れば MemoExoticComponent 化で
    // 壊れ得るため blocking 維持。
    if has_blocking_value_usage(index, name) {
        return None;
    }
    Some(CompatibleApiModification {
        name: name.to_string(),
        kind: kind.to_string(),
        file: new_path.to_string(),
        old_signature: Some(old_sig.to_string()),
        new_signature: Some(new_sig.to_string()),
        reason: "react_component_wrapper".to_string(),
    })
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn detect_object_members_compatible_mod(
    dir: &str,
    base: &str,
    old_path: &str,
    new_path: &str,
    name: &str,
    kind: &str,
    old_sig: &str,
    new_sig: &str,
    lang_id: Option<crate::language::LangId>,
) -> Option<CompatibleApiModification> {
    use crate::language::LangId;
    let lang =
        lang_id.filter(|l| matches!(l, LangId::Typescript | LangId::Tsx | LangId::Javascript))?;
    if !crate::engine::impact::is_safe_diff_path(old_path)
        || !crate::engine::impact::is_safe_diff_path(new_path)
    {
        return None;
    }
    validate_git_revision(base, "--base").ok()?;
    validate_git_revision(old_path, "diff file path").ok()?;
    let old_output = std::process::Command::new("git")
        .args(["show", &format!("{base}:{old_path}")])
        .current_dir(dir)
        .output()
        .ok()?;
    if !old_output.status.success() {
        return None;
    }
    let new_full = std::path::Path::new(dir).join(new_path);
    let new_utf8 = camino::Utf8Path::from_path(&new_full)?;
    let new_source = parser::read_file(new_utf8).ok()?;
    let old_keys = extract_object_member_keys(&old_output.stdout, lang, name)?;
    let new_keys = extract_object_member_keys(&new_source, lang, name)?;
    if old_keys.record_keys.is_some() != new_keys.record_keys.is_some() {
        return None;
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
    // 削除された schema キー (old にあって new にない)。各キーへの member access が repo
    // 全体で残っていれば破壊的なので blocking 維持。
    for key in removed_members {
        if key_has_member_access_ref(dir, key) {
            return None;
        }
    }
    Some(CompatibleApiModification {
        name: name.to_string(),
        kind: kind.to_string(),
        file: new_path.to_string(),
        old_signature: Some(old_sig.to_string()),
        new_signature: Some(new_sig.to_string()),
        reason: "unused_object_members".to_string(),
    })
}

/// TS/TSX のトップレベル exported function で、末尾 optional/default 引数追加だけを
/// compatible_modified (`trailing_optional_params`) として判定する。
///
/// 次をすべて満たす場合だけ降格する:
/// - 関数シンボルで、old/new ともトップレベル関数として一意に取得できる
/// - 関数名・型パラメータ・戻り値など parameters 外の signature が不変
/// - 既存引数の順序・型・optional/default 指定が不変
/// - 追加された末尾引数がすべて optional (`?`) または default value 付き
///
/// class method / const arrow function / import 型の解決などは対象外にして blocking を維持する。
/// false negative (破壊的変更の見逃し) を避けるため、AST 取得や git show に失敗した場合も None。
#[allow(clippy::too_many_arguments)]
pub(crate) fn detect_trailing_optional_params_compatible_mod(
    dir: &str,
    base: &str,
    old_path: &str,
    new_path: &str,
    name: &str,
    kind: &str,
    old_sig: &str,
    new_sig: &str,
    lang_id: Option<crate::language::LangId>,
) -> Option<CompatibleApiModification> {
    use crate::language::LangId;
    let lang = lang_id.filter(|l| matches!(l, LangId::Typescript | LangId::Tsx))?;
    if kind != "function" || name.contains('.') {
        return None;
    }
    if !crate::engine::impact::is_safe_diff_path(old_path)
        || !crate::engine::impact::is_safe_diff_path(new_path)
    {
        return None;
    }
    validate_git_revision(base, "--base").ok()?;
    validate_git_revision(old_path, "diff file path").ok()?;

    let old_output = std::process::Command::new("git")
        .args(["show", &format!("{base}:{old_path}")])
        .current_dir(dir)
        .output()
        .ok()?;
    if !old_output.status.success() {
        return None;
    }
    let old_source = old_output.stdout;
    let old_tree = parser::parse_source(&old_source, lang).ok()?;

    let new_full = std::path::Path::new(dir).join(new_path);
    let new_utf8 = camino::Utf8Path::from_path(&new_full)?;
    let new_source = parser::read_file(new_utf8).ok()?;
    let new_tree = parser::parse_source(&new_source, lang).ok()?;

    let old_fn = find_top_level_function_by_name(old_tree.root_node(), &old_source, name)?;
    let new_fn = find_top_level_function_by_name(new_tree.root_node(), &new_source, name)?;
    let old_parts = ts_function_signature_parts(old_fn, &old_source)?;
    let new_parts = ts_function_signature_parts(new_fn, &new_source)?;

    if old_parts.head != new_parts.head || old_parts.tail != new_parts.tail {
        return None;
    }
    if !ts_params_prefix_same_with_optional_tail(&old_parts.params, &new_parts.params) {
        return None;
    }

    Some(CompatibleApiModification {
        name: name.to_string(),
        kind: kind.to_string(),
        file: new_path.to_string(),
        old_signature: Some(old_sig.to_string()),
        new_signature: Some(new_sig.to_string()),
        reason: "trailing_optional_params".to_string(),
    })
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
    let obj = unwrap_to_object_literal(value)?;
    collect_object_keys(obj, source)
}

/// `expr as const` / `expr satisfies T` をはがして object literal ノードを返す。
pub(crate) fn unwrap_to_object_literal(node: tree_sitter::Node) -> Option<tree_sitter::Node> {
    let mut cur = node;
    loop {
        match cur.kind() {
            "object" => return Some(cur),
            "as_expression" | "satisfies_expression" => {
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
    let mut record_member_keys: Option<HashSet<String>> = None;
    let mut has_object_value = false;
    let mut has_non_object_value = false;
    let mut cursor = obj.walk();
    for child in obj.named_children(&mut cursor) {
        match child.kind() {
            "pair" => {
                let key = child.child_by_field_name("key")?;
                top_level_keys.insert(object_key_text(key, source)?);
                if let Some(value) = child.child_by_field_name("value")
                    && let Some(nested) = unwrap_to_object_literal(value)
                {
                    has_object_value = true;
                    let nested_keys = collect_flat_object_keys(nested, source)?;
                    match &record_member_keys {
                        Some(existing) if existing != &nested_keys => return None,
                        Some(_) => {}
                        None => record_member_keys = Some(nested_keys),
                    }
                } else {
                    has_non_object_value = true;
                }
            }
            "shorthand_property_identifier" => {
                top_level_keys.insert(child.utf8_text(source).ok()?.to_string());
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
        });
    }
    Some(ObjectMemberKeys {
        member_keys: top_level_keys,
        record_keys: None,
    })
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

/// `key` への member access (`.key` / `['key']` / `["key"]`) が repo 全体に残っているか。
/// 解析失敗は保守的に true (残存ありとみなし blocking)。
pub(crate) fn key_has_member_access_ref(dir: &str, key: &str) -> bool {
    if key.is_empty() {
        return true;
    }
    let files = match crate::engine::refs::collect_files(std::path::Path::new(dir), None) {
        Ok(files) => files,
        Err(_) => return true,
    };
    files
        .into_par_iter()
        .any(|path| file_has_member_access_ref(path.as_path(), key).unwrap_or(true))
}

pub(crate) fn file_has_member_access_ref(path: &std::path::Path, key: &str) -> Result<bool> {
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
                    source_has_member_access_ref(source.as_bytes(), lang, key)
                }
                Ok(_) | Err(_) => Ok(false),
            };
        }
        Ok(_) | Err(_) => return Ok(false),
    };
    let source = parser::read_file(utf8_path)?;
    source_has_member_access_ref(source.as_bytes(), lang, key)
}

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

pub(crate) fn ast_has_member_access_ref(node: tree_sitter::Node, source: &[u8], key: &str) -> bool {
    if node_is_member_access_ref(node, source, key) {
        return true;
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .any(|child| ast_has_member_access_ref(child, source, key))
}

pub(crate) fn node_is_member_access_ref(node: tree_sitter::Node, source: &[u8], key: &str) -> bool {
    match node.kind() {
        "member_expression" => {
            node.child_by_field_name("property")
                .and_then(|property| property.utf8_text(source).ok())
                == Some(key)
        }
        "subscript_expression" => {
            node.child_by_field_name("index")
                .filter(|index| index.kind() == "string")
                .and_then(|index| static_js_string_text(index, source))
                == Some(key)
        }
        // destructuring (`const { beta } = config;`) も member の実利用。
        // shorthand (`{ beta }` / `{ beta = 1 }` の左辺) は
        // shorthand_property_identifier_pattern、rename (`{ beta: b }`) は pair_pattern の
        // key に現れる。見落とすと破壊的な member 削除が unused_object_members に降格する。
        "shorthand_property_identifier_pattern" => node.utf8_text(source).ok() == Some(key),
        "pair_pattern" => {
            let Some(key_node) = node.child_by_field_name("key") else {
                return false;
            };
            match key_node.kind() {
                "string" => static_js_string_text(key_node, source) == Some(key),
                _ => key_node.utf8_text(source).ok() == Some(key),
            }
        }
        _ => false,
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
/// `base` / `file_path` は `validate_git_revision` で検証する (codex 指摘: 既存の
/// `extract_exported_symbols_from_git` と同じ防御を行わないと `--diff` / stdin 経路で
/// 未検証の `base` がここに到達し得る)。
pub(crate) fn old_top_level_function_has_empty_parameters(
    dir: &str,
    base: &str,
    file_path: &str,
    lang_id: crate::language::LangId,
    fn_name: &str,
) -> bool {
    if validate_git_revision(base, "--base").is_err()
        || validate_git_revision(file_path, "diff file path").is_err()
    {
        return false;
    }
    let output = std::process::Command::new("git")
        .args(["show", &format!("{base}:{file_path}")])
        .current_dir(dir)
        .output();
    let Ok(output) = output else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let source = output.stdout;
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
