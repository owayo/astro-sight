//! Python 固有のシグネチャ解析と互換 API 変更判定ヘルパー (末尾 optional/default 引数追加)。

use crate::language::LangId;
use crate::models::review::CompatibleApiModification;

use super::normalize_signature_whitespace;
use super::source_pair::{CompatibleModSite, SignatureSourceCache};

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
pub(crate) fn detect_python_trailing_optional_params_compatible_mod(
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache,
) -> Option<CompatibleApiModification> {
    let lang = site.lang_in(&[LangId::Python])?;
    if site.kind != "function" && site.kind != "method" {
        return None;
    }
    let src = sources.get(site)?;
    let (old_tree, new_tree) = src.parse_pair(lang)?;

    let old_fn = find_python_function_by_name(old_tree.root_node(), &src.old, site.name)?;
    let new_fn = find_python_function_by_name(new_tree.root_node(), &src.new, site.name)?;
    let old_parts = python_function_signature_parts(old_fn, &src.old)?;
    let new_parts = python_function_signature_parts(new_fn, &src.new)?;

    if old_parts.head != new_parts.head || old_parts.tail != new_parts.tail {
        return None;
    }
    // デコレータの差分は呼び出し互換に影響しうる (`@staticmethod` ↔ `@classmethod` 等)。
    // 内容まで安全に分類するのは難しいため、差があれば保守的に blocking 維持する。
    if old_parts.decorators != new_parts.decorators {
        return None;
    }
    if !python_params_compatible_addition(&old_parts.parts, &new_parts.parts) {
        return None;
    }

    Some(site.compatible("trailing_optional_params"))
}

/// Python の関数シグネチャ内で、純粋な隣接文字列リテラルの分割・結合だけが行われた
/// 変更を `equivalent_implicit_string_concat` として降格する。
///
/// AST の leaf token 列を比較し、`string` / `concatenated_string` だけを同じ canonical
/// value token へ置換する。f-string、escape、異種 prefix、演算子や参照を含む式は評価せず
/// blocking を維持する。少なくとも片側に暗黙連結が実在するときだけ適用するため、他の
/// Python シグネチャ変更をこの判定器が横取りしない。
pub(crate) fn detect_python_implicit_string_concat_compatible_mod(
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache,
) -> Option<CompatibleApiModification> {
    let lang = site.lang_in(&[LangId::Python])?;
    if site.kind != "function" && site.kind != "method" {
        return None;
    }
    let src = sources.get(site)?;
    let (old_tree, new_tree) = src.parse_pair(lang)?;
    if old_tree.root_node().has_error() || new_tree.root_node().has_error() {
        return None;
    }

    let old_fn = find_python_function_by_name(old_tree.root_node(), &src.old, site.name)?;
    let new_fn = find_python_function_by_name(new_tree.root_node(), &src.new, site.name)?;
    let old_sig = canonical_python_function_signature(old_fn, &src.old)?;
    let new_sig = canonical_python_function_signature(new_fn, &src.new)?;

    if old_sig.decorators != new_sig.decorators
        || old_sig.tokens != new_sig.tokens
        || !(old_sig.had_implicit_concat || new_sig.had_implicit_concat)
    {
        return None;
    }
    Some(site.compatible("equivalent_implicit_string_concat"))
}

#[derive(Debug, PartialEq, Eq)]
struct CanonicalPythonSignature {
    tokens: Vec<u8>,
    decorators: Vec<String>,
    had_implicit_concat: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct PythonLiteralValue {
    prefix: Vec<u8>,
    content: Vec<u8>,
    had_implicit_concat: bool,
}

/// body を除く function_definition の全 leaf token を、境界衝突しない長さ付き形式へ直す。
/// 走査は反復式とし、病的に深いシグネチャでも Rust stack を消費しない。
fn canonical_python_function_signature(
    fn_node: tree_sitter::Node<'_>,
    source: &[u8],
) -> Option<CanonicalPythonSignature> {
    const MAX_SIGNATURE_NODES: usize = 100_000;

    let body = fn_node.child_by_field_name("body")?;
    let decorators = python_collect_decorators(fn_node, source);
    let mut tokens = Vec::new();
    let mut had_implicit_concat = false;
    let mut visited = 0usize;
    let mut stack = Vec::new();
    for index in (0..fn_node.child_count()).rev() {
        let child = fn_node.child(index)?;
        if !same_node(child, body) {
            stack.push(child);
        }
    }

    while let Some(node) = stack.pop() {
        visited += 1;
        if visited > MAX_SIGNATURE_NODES {
            return None;
        }

        if matches!(
            node.kind(),
            "string" | "concatenated_string" | "parenthesized_expression"
        ) && let Some(value) = evaluate_python_literal(node, source, 0)
        {
            had_implicit_concat |= value.had_implicit_concat;
            push_canonical_token(&mut tokens, "python_string_prefix", &value.prefix);
            push_canonical_token(&mut tokens, "python_string_value", &value.content);
            continue;
        }
        // string 系 node を安全に評価できない場合は、部分的に子だけを正規化せず全体を諦める。
        // 特に f-string の interpolation を leaf token 比較へ流して互換扱いしないためのガード。
        if matches!(node.kind(), "string" | "concatenated_string") {
            return None;
        }

        if node.child_count() == 0 {
            if node.kind() == "," && is_python_trailing_comma(node) {
                continue;
            }
            let text = source.get(node.start_byte()..node.end_byte())?;
            push_canonical_token(&mut tokens, node.kind(), text);
            continue;
        }
        for index in (0..node.child_count()).rev() {
            stack.push(node.child(index)?);
        }
    }

    Some(CanonicalPythonSignature {
        tokens,
        decorators,
        had_implicit_concat,
    })
}

/// 複数行整形で付く末尾カンマだけを無視する。tuple/list/dict のカンマや、引数間の
/// カンマは意味・arity に関わるため対象外。
fn is_python_trailing_comma(node: tree_sitter::Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if !matches!(parent.kind(), "parameters" | "argument_list") {
        return false;
    }
    let count = parent.named_child_count();
    let Some(last_index) = count
        .checked_sub(1)
        .and_then(|value| u32::try_from(value).ok())
    else {
        return false;
    };
    parent
        .named_child(last_index)
        .is_some_and(|last| node.start_byte() >= last.end_byte())
}

fn same_node(left: tree_sitter::Node<'_>, right: tree_sitter::Node<'_>) -> bool {
    left.kind() == right.kind()
        && left.start_byte() == right.start_byte()
        && left.end_byte() == right.end_byte()
}

fn push_canonical_token(out: &mut Vec<u8>, kind: &str, value: &[u8]) {
    out.extend_from_slice(kind.len().to_string().as_bytes());
    out.push(b':');
    out.extend_from_slice(kind.as_bytes());
    out.push(b'=');
    out.extend_from_slice(value.len().to_string().as_bytes());
    out.push(b':');
    out.extend_from_slice(value);
    out.push(b';');
}

fn evaluate_python_literal(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    depth: usize,
) -> Option<PythonLiteralValue> {
    const MAX_LITERAL_DEPTH: usize = 32;
    if depth > MAX_LITERAL_DEPTH {
        return None;
    }

    match node.kind() {
        "string" => parse_python_string_literal(node, source),
        "concatenated_string" => {
            let mut prefix: Option<Vec<u8>> = None;
            let mut content = Vec::new();
            let mut count = 0usize;
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() != "string" {
                    return None;
                }
                let value = evaluate_python_literal(child, source, depth + 1)?;
                if let Some(expected) = &prefix {
                    if expected != &value.prefix {
                        return None;
                    }
                } else {
                    prefix = Some(value.prefix);
                }
                content.extend(value.content);
                count += 1;
            }
            (count >= 2).then_some(PythonLiteralValue {
                prefix: prefix?,
                content,
                had_implicit_concat: true,
            })
        }
        "parenthesized_expression" => {
            let mut cursor = node.walk();
            let named = node.named_children(&mut cursor).collect::<Vec<_>>();
            let [inner] = named.as_slice() else {
                return None;
            };
            evaluate_python_literal(*inner, source, depth + 1)
        }
        _ => None,
    }
}

/// escape / interpolation を持たない文字列だけを byte 値として読む。quote の種類は値に
/// 影響しないため除外し、prefix は小文字化して str / bytes / raw の混同を防ぐ。
fn parse_python_string_literal(
    node: tree_sitter::Node<'_>,
    source: &[u8],
) -> Option<PythonLiteralValue> {
    let mut prefix = None;
    let mut content = Vec::new();
    let mut saw_end = false;
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "string_start" if prefix.is_none() => {
                let start = source.get(child.start_byte()..child.end_byte())?;
                prefix = Some(python_string_prefix(start)?);
            }
            "string_content" => {
                if child.child_count() != 0 {
                    return None;
                }
                content.extend_from_slice(source.get(child.start_byte()..child.end_byte())?);
            }
            "string_end" if !saw_end => saw_end = true,
            // escape_sequence / interpolation / 不明 node は評価しない。
            _ => return None,
        }
    }
    Some(PythonLiteralValue {
        prefix: prefix.filter(|_| saw_end)?,
        content,
        had_implicit_concat: false,
    })
}

fn python_string_prefix(start: &[u8]) -> Option<Vec<u8>> {
    let quote_at = start.iter().position(|byte| matches!(byte, b'\'' | b'"'))?;
    let quotes = start.get(quote_at..)?;
    let quote = *quotes.first()?;
    if !(quotes.len() == 1 || quotes.len() == 3) || quotes.iter().any(|byte| *byte != quote) {
        return None;
    }
    let prefix = start
        .get(..quote_at)?
        .iter()
        .map(u8::to_ascii_lowercase)
        .collect::<Vec<_>>();
    matches!(prefix.as_slice(), b"" | b"u" | b"r" | b"b" | b"br" | b"rb").then_some(prefix)
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
    decorators: Vec<String>,
}

/// Python の `function_definition` を head (def 〜 `(` 直前) / parameters / tail
/// (戻り値型 + `:` まで) に分け、body 直前で切る。`decorated_definition` 配下の
/// `function_definition` を受け取った場合は親のデコレータ列も併せて返し、
/// `@staticmethod` ↔ `@classmethod` のような呼出側互換に影響する変更を
/// 降格しないようにする。
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
    let decorators = python_collect_decorators(fn_node, source);
    Some(PyFunctionSignatureParts {
        head,
        tail,
        parts,
        decorators,
    })
}

/// `function_definition` の親が `decorated_definition` の場合、デコレータ各 named child の
/// 正規化テキストを返す。decorator が無ければ空 Vec。
pub(crate) fn python_collect_decorators(
    fn_node: tree_sitter::Node<'_>,
    source: &[u8],
) -> Vec<String> {
    let Some(parent) = fn_node.parent() else {
        return Vec::new();
    };
    if parent.kind() != "decorated_definition" {
        return Vec::new();
    }
    let mut decorators = Vec::new();
    let mut cursor = parent.walk();
    for child in parent.named_children(&mut cursor) {
        if child.kind() == "decorator"
            && let Some(text) = source.get(child.start_byte()..child.end_byte())
        {
            decorators.push(normalize_signature_whitespace(text));
        }
    }
    decorators
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
/// 同名候補が複数見つかった場合は `None` を返す (どの定義が `old_sig` / `new_sig` の対象か
/// 一意に決められず、誤った互換降格を起こすため、blocking を維持する)。
pub(crate) fn find_python_function_by_name<'a>(
    root: tree_sitter::Node<'a>,
    source: &[u8],
    name: &str,
) -> Option<tree_sitter::Node<'a>> {
    let matches = collect_python_function_candidates(root, source, name);
    if matches.len() == 1 {
        Some(matches[0])
    } else {
        None
    }
}

/// `find_python_function_by_name` の内部実装。マッチした全候補を返し、呼び出し側で
/// 件数を判定する。
fn collect_python_function_candidates<'a>(
    root: tree_sitter::Node<'a>,
    source: &[u8],
    name: &str,
) -> Vec<tree_sitter::Node<'a>> {
    let (class_name, fn_name) = match name.split_once('.') {
        Some((cls, fnm)) if !cls.is_empty() && !fnm.is_empty() => (Some(cls), fnm),
        _ => (None, name),
    };

    let mut matches = Vec::new();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
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
                        matches.push(fn_node);
                    }
                }
            }
            continue;
        }
        if let Some(fn_node) = python_function_definition_with_name(child, source, fn_name) {
            matches.push(fn_node);
        }
    }
    matches
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
    use crate::engine::parser;
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

    fn canonical(src: &str, name: &str) -> Option<CanonicalPythonSignature> {
        let tree = parse(src);
        let fn_node = find_python_function_by_name(tree.root_node(), src.as_bytes(), name)?;
        canonical_python_function_signature(fn_node, src.as_bytes())
    }

    #[test]
    fn implicit_concat_is_canonicalized_in_defaults_and_annotations() {
        let old_default = canonical(
            "def f(value: str = describe(\"alpha beta\")) -> None:\n    pass\n",
            "f",
        )
        .unwrap();
        let new_default = canonical(
            concat!(
                "def f(\n",
                "    value: str = describe((\"alpha \" \"beta\")),\n",
                ") -> None:\n",
                "    pass\n"
            ),
            "f",
        )
        .unwrap();
        assert_eq!(old_default.tokens, new_default.tokens);
        assert!(new_default.had_implicit_concat);

        let old_annotation = canonical(
            "def f(value: \"AlphaBeta\") -> \"GammaDelta\":\n    pass\n",
            "f",
        )
        .unwrap();
        let new_annotation = canonical(
            "def f(value: \"Alpha\" \"Beta\") -> \"Gamma\" \"Delta\":\n    pass\n",
            "f",
        )
        .unwrap();
        assert_eq!(old_annotation.tokens, new_annotation.tokens);
        assert!(new_annotation.had_implicit_concat);
    }

    #[test]
    fn implicit_concat_rejects_fstrings_escapes_and_mixed_prefixes() {
        assert!(canonical("def f(value=f\"a{x}\" f\"b{y}\"):\n    pass\n", "f").is_none());
        assert!(canonical("def f(value=\"a\\n\" \"b\"):\n    pass\n", "f").is_none());
        assert!(canonical("def f(value=b\"a\" \"b\"):\n    pass\n", "f").is_none());
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

    #[test]
    fn duplicate_top_level_function_is_ambiguous() {
        // 同一モジュール内に同名トップレベル関数が複数あるとどの定義が対象か
        // 確定できないため、blocking 維持 (None) を期待する。
        let src = "def f(a):\n    return a\n\ndef f(a, b):\n    return a + b\n";
        let tree = parse(src);
        assert!(find_python_function_by_name(tree.root_node(), src.as_bytes(), "f").is_none());
    }

    #[test]
    fn decorator_diff_is_blocking() {
        // @staticmethod ↔ @classmethod のようなデコレータ変更は呼び出し互換に影響しうるため、
        // default 引数追加と同時でも compatible_modified に降格しない。
        let old_src = "@staticmethod\ndef f(a):\n    return a\n";
        let new_src = "@classmethod\ndef f(a, b=None):\n    return a\n";
        let old_tree = parse(old_src);
        let new_tree = parse(new_src);
        let old_fn =
            find_python_function_by_name(old_tree.root_node(), old_src.as_bytes(), "f").unwrap();
        let new_fn =
            find_python_function_by_name(new_tree.root_node(), new_src.as_bytes(), "f").unwrap();
        let old_parts = python_function_signature_parts(old_fn, old_src.as_bytes()).unwrap();
        let new_parts = python_function_signature_parts(new_fn, new_src.as_bytes()).unwrap();
        assert_ne!(old_parts.decorators, new_parts.decorators);
    }

    #[test]
    fn same_decorator_with_added_default_param_is_compatible() {
        // 同じデコレータ + default 引数追加なら降格対象。
        let old_src = "@staticmethod\ndef f(a):\n    return a\n";
        let new_src = "@staticmethod\ndef f(a, b=None):\n    return a\n";
        let old_tree = parse(old_src);
        let new_tree = parse(new_src);
        let old_fn =
            find_python_function_by_name(old_tree.root_node(), old_src.as_bytes(), "f").unwrap();
        let new_fn =
            find_python_function_by_name(new_tree.root_node(), new_src.as_bytes(), "f").unwrap();
        let old_parts = python_function_signature_parts(old_fn, old_src.as_bytes()).unwrap();
        let new_parts = python_function_signature_parts(new_fn, new_src.as_bytes()).unwrap();
        assert_eq!(old_parts.decorators, new_parts.decorators);
        assert!(python_params_compatible_addition(
            &old_parts.parts,
            &new_parts.parts
        ));
    }
}
