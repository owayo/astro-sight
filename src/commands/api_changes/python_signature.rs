//! Python 固有のシグネチャ解析と互換 API 変更判定ヘルパー (末尾 optional/default 引数追加)。

use crate::engine::parser;
use crate::language::LangId;
use crate::models::review::CompatibleApiModification;

use super::super::git_input::validate_git_revision;
use super::normalize_signature_whitespace;

/// Python のトップレベル関数 / モジュール直下のクラスメソッドで、
/// 末尾 keyword-only / default 引数追加だけを `trailing_optional_params` として降格する。
///
/// 次をすべて満たす場合だけ降格する:
/// - 関数または method シンボルで、old/new とも対象ノードとして一意に取得できる
/// - 関数名・デコレータ・戻り値型注釈・head が不変
/// - 既存引数の順序・型注釈・default 指定が不変
/// - 追加された末尾引数がすべて以下のいずれか
///   - `default_parameter` / `typed_default_parameter` (positional default 付き、末尾追加)
///   - `keyword_separator` (`*`) は追加可。後続は default 付きの keyword-only 引数のみ
/// - `*args` / `**kwargs` / `/` (`positional_separator`) の新規追加は対象外
///
/// 抽出失敗・rest 引数の混入・既存 `**kwargs` の前に新規 kwonly 引数を差し込む形は None を返し
/// blocking を維持する (false negative 回避)。
#[allow(clippy::too_many_arguments)]
pub(crate) fn detect_python_trailing_optional_params_compatible_mod(
    dir: &str,
    base: &str,
    old_path: &str,
    new_path: &str,
    name: &str,
    kind: &str,
    old_sig: &str,
    new_sig: &str,
    lang_id: Option<LangId>,
) -> Option<CompatibleApiModification> {
    let lang = lang_id.filter(|l| matches!(l, LangId::Python))?;
    if kind != "function" && kind != "method" {
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

    let old_fn = find_python_function_by_name(old_tree.root_node(), &old_source, name)?;
    let new_fn = find_python_function_by_name(new_tree.root_node(), &new_source, name)?;
    let old_parts = python_function_signature_parts(old_fn, &old_source)?;
    let new_parts = python_function_signature_parts(new_fn, &new_source)?;

    if old_parts.head != new_parts.head || old_parts.tail != new_parts.tail {
        return None;
    }
    if !python_params_compatible_addition(&old_parts.parts, &new_parts.parts) {
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
pub(crate) enum PyParamPart {
    /// 通常の引数 (identifier / typed_parameter / default_parameter / typed_default_parameter)
    Param(PyFunctionParam),
    /// bare `*` — 以降を keyword-only にする境界
    KeywordSeparator,
    /// `/` — 以前を positional-only にする境界
    PositionalSeparator,
    /// `*args`
    VarArgs(String),
    /// `**kwargs`
    KwArgs(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PyFunctionParam {
    normalized: String,
    has_default: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PyFunctionSignatureParts {
    head: String,
    tail: String,
    parts: Vec<PyParamPart>,
}

/// Python の `function_definition` を head (def 〜 `(` 直前) / parameters / tail
/// (戻り値型 + `:` まで) に分け、body 直前で切る。デコレータは比較対象外
/// (decorated_definition の親はスコープに含めない)。
pub(crate) fn python_function_signature_parts(
    fn_node: tree_sitter::Node<'_>,
    source: &[u8],
) -> Option<PyFunctionSignatureParts> {
    let params = fn_node.child_by_field_name("parameters")?;
    let body = fn_node.child_by_field_name("body")?;
    let sig_start = fn_node.start_byte();
    let head = normalize_signature_whitespace(source.get(sig_start..params.start_byte())?);
    let tail = normalize_signature_whitespace(source.get(params.end_byte()..body.start_byte())?);
    let parts = python_function_params(params, source)?;
    Some(PyFunctionSignatureParts { head, tail, parts })
}

/// parameters 直下の named child を順番に収集する。判定不能な kind が混ざる場合は
/// None にして blocking を維持する。
pub(crate) fn python_function_params(
    params: tree_sitter::Node<'_>,
    source: &[u8],
) -> Option<Vec<PyParamPart>> {
    let mut result = Vec::new();
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        match child.kind() {
            "identifier" | "typed_parameter" => {
                let text = source.get(child.start_byte()..child.end_byte())?;
                result.push(PyParamPart::Param(PyFunctionParam {
                    normalized: normalize_signature_whitespace(text),
                    has_default: false,
                }));
            }
            "default_parameter" | "typed_default_parameter" => {
                let text = source.get(child.start_byte()..child.end_byte())?;
                result.push(PyParamPart::Param(PyFunctionParam {
                    normalized: normalize_signature_whitespace(text),
                    has_default: true,
                }));
            }
            "list_splat_pattern" => {
                let text = source.get(child.start_byte()..child.end_byte())?;
                result.push(PyParamPart::VarArgs(normalize_signature_whitespace(text)));
            }
            "dictionary_splat_pattern" => {
                let text = source.get(child.start_byte()..child.end_byte())?;
                result.push(PyParamPart::KwArgs(normalize_signature_whitespace(text)));
            }
            "keyword_separator" => result.push(PyParamPart::KeywordSeparator),
            "positional_separator" => result.push(PyParamPart::PositionalSeparator),
            // 想定外の kind は blocking 維持 (false negative 回避)
            _ => return None,
        }
    }
    Some(result)
}

/// old の全 parts が new の prefix と一致し、追加された parts が以下を満たすなら true:
/// - 末尾の追加 parts は default 付き Param と KeywordSeparator のみで構成される
/// - 追加 parts に実 Param が 1 つ以上含まれる
/// - VarArgs / KwArgs / PositionalSeparator の追加は不可
/// - 既存 `**kwargs` の前に kwonly 引数を差し込む形は対象外
///   (`kwargs` に入っていた名前を正式引数へ吸う可能性があるため)
pub(crate) fn python_params_compatible_addition(
    old_parts: &[PyParamPart],
    new_parts: &[PyParamPart],
) -> bool {
    if new_parts.len() <= old_parts.len() {
        return false;
    }
    for (old, new) in old_parts.iter().zip(new_parts.iter()) {
        if old != new {
            return false;
        }
    }
    let added = &new_parts[old_parts.len()..];

    // 末尾追加: old 末尾が `**kwargs` の場合、`**kwargs` の前への差し込みになるため対象外
    if matches!(old_parts.last(), Some(PyParamPart::KwArgs(_))) {
        return false;
    }

    let mut has_real_param = false;
    for part in added {
        match part {
            PyParamPart::Param(p) => {
                if !p.has_default {
                    return false;
                }
                has_real_param = true;
            }
            PyParamPart::KeywordSeparator => {}
            // VarArgs / KwArgs / PositionalSeparator の追加は呼び出し側の挙動を変えうるため
            // blocking 維持。
            _ => return false,
        }
    }
    has_real_param
}

/// `root` のトップレベル (module 直下、または直下クラス定義配下) にある
/// `function_definition` のうち、name が一致するものを返す。
/// `decorated_definition` 配下の `function_definition` も対象。
/// ネストしたローカル関数 (関数内 def) は対象外。
pub(crate) fn find_python_function_by_name<'a>(
    root: tree_sitter::Node<'a>,
    source: &[u8],
    name: &str,
) -> Option<tree_sitter::Node<'a>> {
    // `name` が `Class.method` 形式ならクラス配下を探索、bare 名ならモジュール直下。
    let (class_name, fn_name) = match name.split_once('.') {
        Some((cls, fnm)) if !cls.is_empty() && !fnm.is_empty() => (Some(cls), fnm),
        _ => (None, name),
    };

    // module 直下を走査して、対象 (module 直下 fn / class 配下 fn) を 1 つ返す。
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        // class 配下の method を探すケース
        if let Some(cls) = class_name {
            let class_node = if child.kind() == "decorated_definition" {
                child
                    .child_by_field_name("definition")
                    .filter(|d| d.kind() == "class_definition")
            } else if child.kind() == "class_definition" {
                Some(child)
            } else {
                None
            };
            if let Some(cls_node) = class_node
                && let Some(cls_name_node) = cls_node.child_by_field_name("name")
                && let Some(bytes) =
                    source.get(cls_name_node.start_byte()..cls_name_node.end_byte())
                && let Ok(cls_text) = std::str::from_utf8(bytes)
                && cls_text == cls
                && let Some(body) = cls_node.child_by_field_name("body")
            {
                let mut sub_cursor = body.walk();
                for body_child in body.children(&mut sub_cursor) {
                    if let Some(fn_node) =
                        python_function_definition_with_name(body_child, source, fn_name)
                    {
                        return Some(fn_node);
                    }
                }
            }
            continue;
        }
        // module 直下 fn を探すケース
        if let Some(fn_node) = python_function_definition_with_name(child, source, fn_name) {
            return Some(fn_node);
        }
    }
    None
}

/// `node` 自身またはその直下の `function_definition` で name が一致するなら返す。
/// `decorated_definition` も 1 段だけはがす。
fn python_function_definition_with_name<'a>(
    node: tree_sitter::Node<'a>,
    source: &[u8],
    name: &str,
) -> Option<tree_sitter::Node<'a>> {
    let fn_node = if node.kind() == "decorated_definition" {
        node.child_by_field_name("definition")
            .filter(|d| d.kind() == "function_definition")?
    } else if node.kind() == "function_definition" {
        node
    } else {
        return None;
    };
    let name_node = fn_node.child_by_field_name("name")?;
    let bytes = source.get(name_node.start_byte()..name_node.end_byte())?;
    let decl_name = std::str::from_utf8(bytes).ok()?;
    if decl_name == name {
        Some(fn_node)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::LangId;

    fn parse(src: &str) -> tree_sitter::Tree {
        parser::parse_source(src.as_bytes(), LangId::Python).unwrap()
    }

    fn parts(src: &str, name: &str) -> Vec<PyParamPart> {
        let tree = parse(src);
        let fn_node = find_python_function_by_name(tree.root_node(), src.as_bytes(), name).unwrap();
        python_function_signature_parts(fn_node, src.as_bytes())
            .unwrap()
            .parts
    }

    #[test]
    fn add_trailing_default_positional_is_compatible() {
        let old = "def f(a):\n    return a\n";
        let new = "def f(a, b=None):\n    return a\n";
        let o = parts(old, "f");
        let n = parts(new, "f");
        assert!(python_params_compatible_addition(&o, &n));
    }

    #[test]
    fn add_trailing_kwonly_with_default_is_compatible() {
        let old = "def f(a):\n    return a\n";
        let new = "def f(a, *, flag=False):\n    return a\n";
        let o = parts(old, "f");
        let n = parts(new, "f");
        assert!(python_params_compatible_addition(&o, &n));
    }

    #[test]
    fn add_trailing_kwonly_without_default_is_blocking() {
        let old = "def f(a):\n    return a\n";
        let new = "def f(a, *, flag):\n    return a\n";
        let o = parts(old, "f");
        let n = parts(new, "f");
        assert!(!python_params_compatible_addition(&o, &n));
    }

    #[test]
    fn insert_kwonly_before_existing_kwargs_is_blocking() {
        let old = "def f(a, **kw):\n    return a\n";
        let new = "def f(a, *, b=None, **kw):\n    return a\n";
        let o = parts(old, "f");
        let n = parts(new, "f");
        assert!(!python_params_compatible_addition(&o, &n));
    }

    #[test]
    fn changed_existing_param_is_blocking() {
        let old = "def f(a):\n    return a\n";
        let new = "def f(a: int, b=None):\n    return a\n";
        let o = parts(old, "f");
        let n = parts(new, "f");
        assert!(!python_params_compatible_addition(&o, &n));
    }

    #[test]
    fn class_method_is_resolvable_by_qualified_name() {
        let src = "class C:\n    def f(self, a):\n        return a\n";
        let tree = parse(src);
        let fn_node =
            find_python_function_by_name(tree.root_node(), src.as_bytes(), "C.f").unwrap();
        assert_eq!(
            fn_node
                .child_by_field_name("name")
                .unwrap()
                .utf8_text(src.as_bytes())
                .unwrap(),
            "f"
        );
    }

    #[test]
    fn nested_function_is_ignored() {
        let src = "def outer():\n    def inner():\n        return 1\n    return inner\n";
        let tree = parse(src);
        assert!(find_python_function_by_name(tree.root_node(), src.as_bytes(), "inner").is_none());
    }

    #[test]
    fn decorated_top_level_function_is_resolvable() {
        let src = "@decorator\ndef f(a):\n    return a\n";
        let tree = parse(src);
        assert!(find_python_function_by_name(tree.root_node(), src.as_bytes(), "f").is_some());
    }
}
