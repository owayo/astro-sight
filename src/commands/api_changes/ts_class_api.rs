//! TypeScript のクラスと enum の公開の形、関数・メソッドのオーバーロード (Issue #286)。
//!
//! 宣言ファイル (`.d.ts`) には、クラスの公開のフィールド (パラメータプロパティを含む)・アクセサ・
//! index signature・実装の無いメソッドの宣言 (abstract と ambient)・コンストラクタの可視性と
//! シグネチャ、enum のメンバーとその値、関数・メソッドのオーバーロードの宣言 (あれば実装の宣言は
//! 出ない) が出る。これらを API の signature に入れ、変わったら入れ物 (クラス・enum・関数・メソッド)
//! の変更にする。通常のメソッドは従来どおり別のシンボル。`private`・`#private` のメンバーと実装
//! (本体・型注釈の無い初期値以外の式) は入れない。

use std::collections::{BTreeMap, HashMap, HashSet};
use tree_sitter::Node;

use super::signature_tokens::{SigTokens, node_signature_tokens, signature_tokens_in_range};

/// トークンの並びを、コメントと閉じ括弧の前の `,` を除いて空白でつなぐ (整形に依らない)。
fn render(tokens: SigTokens) -> String {
    let tokens: Vec<(String, String)> = tokens
        .0
        .into_iter()
        .filter(|(kind, _)| kind != "comment")
        .collect();
    let mut texts: Vec<&str> = Vec::new();
    for (index, (_, text)) in tokens.iter().enumerate() {
        let trailing = text == ","
            && tokens
                .get(index + 1)
                .is_some_and(|(_, next)| matches!(next.as_str(), ")" | "]" | "}" | ">"));
        if !trailing {
            texts.push(text);
        }
    }
    texts.join(" ")
}

/// `node` の字面 ([`render`])。
fn node_text(node: Node<'_>, source: &[u8]) -> Option<String> {
    Some(render(node_signature_tokens(node, source)?))
}

/// `node` の中の `start`〜`end` の字面 ([`render`])。
fn range_text(node: Node<'_>, source: &[u8], start: usize, end: usize) -> Option<String> {
    Some(render(signature_tokens_in_range(node, source, start, end)?))
}

/// 宣言の名前の字面。
fn name_of<'source>(node: Node<'_>, source: &'source [u8]) -> Option<&'source str> {
    node.child_by_field_name("name")?.utf8_text(source).ok()
}

/// qualname (`Client.send`) の末尾の名前。
fn bare(qualname: &str) -> &str {
    qualname.rsplit('.').next().unwrap_or(qualname)
}

/// `statement` (export 文も) の中の宣言。
fn declaration_of(statement: Node<'_>) -> Node<'_> {
    if statement.kind() == "export_statement" {
        statement
            .child_by_field_name("declaration")
            .unwrap_or(statement)
    } else {
        statement
    }
}

/// メンバー・引数の修飾子。
#[derive(Default)]
struct Modifiers {
    /// アクセス修飾子か `override` を書いたか (`public` も含む。パラメータプロパティの印)。
    property_marker: bool,
    /// `protected` か `private` (`public` と無指定は None)。
    accessibility: Option<String>,
    is_static: bool,
    readonly: bool,
    is_abstract: bool,
    optional: bool,
}

impl Modifiers {
    fn of(node: Node<'_>, source: &[u8]) -> Self {
        let mut found = Self::default();
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "accessibility_modifier" => {
                    found.property_marker = true;
                    let text = child.utf8_text(source).unwrap_or("").trim();
                    if text != "public" {
                        found.accessibility = Some(text.to_owned());
                    }
                }
                "override_modifier" => found.property_marker = true,
                "static" => found.is_static = true,
                "readonly" => found.readonly = true,
                "abstract" => found.is_abstract = true,
                "?" => found.optional = true,
                _ => {}
            }
        }
        found
    }

    fn private(&self) -> bool {
        self.accessibility.as_deref() == Some("private")
    }

    /// 項目の前置き (`static protected abstract readonly `)。`declare`・`override` は宣言ファイルの
    /// 型を変えないので入れない。
    fn prefix(&self) -> String {
        let mut prefix = String::new();
        if self.is_static {
            prefix.push_str("static ");
        }
        if let Some(accessibility) = &self.accessibility {
            prefix.push_str(accessibility);
            prefix.push(' ');
        }
        if self.is_abstract {
            prefix.push_str("abstract ");
        }
        if self.readonly {
            prefix.push_str("readonly ");
        }
        prefix
    }
}

/// メンバーの名前 (文字列の名前は引用符を外す。`'a'` と `a` は同じメンバー)。読めなければ None。
fn member_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    let name = node.child_by_field_name("name")?;
    if name.kind() == "string" {
        let text = name.utf8_text(source).ok()?;
        return Some(text.get(1..text.len().checked_sub(1)?)?.to_owned());
    }
    node_text(name, source)
}

/// `#private` の名前のメンバーか。
fn is_hash_private(node: Node<'_>) -> bool {
    node.child_by_field_name("name")
        .is_some_and(|name| name.kind() == "private_property_identifier")
}

/// 型注釈 (`field` の `: T`) の型の字面。
fn annotation(node: Node<'_>, source: &[u8], field: &str) -> Option<String> {
    let text = node_text(node.child_by_field_name(field)?, source)?;
    Some(text.strip_prefix(": ").unwrap_or(&text).to_owned())
}

/// 型注釈の無いフィールド・引数の型。宣言ファイルは初期値から推論した型を出す。単純なリテラルは
/// その型 (`literal` なら、`readonly` のフィールドのようにリテラルそのものの型) にし、ほかの式は
/// 推論できないので初期値の字面で表す (初期値を変えれば変化にする)。初期値も無ければ `any`。
fn inferred_type(value: Option<Node<'_>>, source: &[u8], literal: bool) -> Option<String> {
    let Some(value) = value else {
        return Some("any".to_owned());
    };
    let text = node_text(value, source)?;
    let widened = match value.kind() {
        "string" => Some("string"),
        "template_string"
            if !value
                .named_children(&mut value.walk())
                .any(|child| child.kind() == "template_substitution") =>
        {
            Some("string")
        }
        "number" => Some(numeric_type(&text)),
        "true" | "false" => Some("boolean"),
        "unary_expression"
            if value
                .child_by_field_name("operator")
                .is_some_and(|operator| operator.kind() == "-") =>
        {
            value
                .child_by_field_name("argument")
                .filter(|argument| argument.kind() == "number")
                .and_then(|argument| argument.utf8_text(source).ok())
                .map(numeric_type)
        }
        _ => None,
    };
    Some(match widened {
        Some(_) if literal => text,
        Some(widened) => widened.to_owned(),
        None => format!("typeof ( {text} )"),
    })
}

/// 数のリテラルの型 (`1n` は bigint)。
fn numeric_type(text: &str) -> &'static str {
    if text.ends_with('n') {
        "bigint"
    } else {
        "number"
    }
}

/// 型注釈の無い分割代入の引数の形。型はこの形から推論される: 取り出すプロパティの名前、入れ子の
/// 形、配列の位置 (空きを含む)、初期値の型 (初期値のある要素は optional)。束縛先のローカルな
/// 変数の名前は型を決めないので `_` にする (`{ y }` と `{ y: z }` は同じ形)。
fn pattern_shape(pattern: Node<'_>, source: &[u8]) -> Option<String> {
    match pattern.kind() {
        "object_pattern" => {
            let mut parts = Vec::new();
            let mut cursor = pattern.walk();
            for child in pattern.named_children(&mut cursor) {
                let part = match child.kind() {
                    "comment" => continue,
                    // `{ x }`
                    "shorthand_property_identifier_pattern" => {
                        format!("{}: _", node_text(child, source)?)
                    }
                    // `{ x = 1 }`
                    "object_assignment_pattern" => {
                        let left = child.child_by_field_name("left")?;
                        let (key, shape) = match left.kind() {
                            "shorthand_property_identifier_pattern" => {
                                (node_text(left, source)?, "_".to_owned())
                            }
                            _ => return None,
                        };
                        format!(
                            "{key}?: {}",
                            defaulted(shape, child.child_by_field_name("right"), source)?
                        )
                    }
                    // `{ x: a }`・`{ x: a = 1 }`・`{ x: { y } }`
                    "pair_pattern" => {
                        let key = node_text(child.child_by_field_name("key")?, source)?;
                        let value = child.child_by_field_name("value")?;
                        if value.kind() == "assignment_pattern" {
                            let shape = pattern_shape(value.child_by_field_name("left")?, source)?;
                            format!(
                                "{key}?: {}",
                                defaulted(shape, value.child_by_field_name("right"), source)?
                            )
                        } else {
                            format!("{key}: {}", pattern_shape(value, source)?)
                        }
                    }
                    "rest_pattern" => "...".to_owned(),
                    _ => node_text(child, source)?,
                };
                parts.push(part);
            }
            Some(format!("{{ {} }}", parts.join(", ")))
        }
        "array_pattern" => {
            // 空きの要素 (`[, a]`) も位置を決めるので、区切りの `,` で要素を分ける。
            let mut parts = Vec::new();
            let mut current: Option<String> = None;
            let mut cursor = pattern.walk();
            for child in pattern.children(&mut cursor) {
                match child.kind() {
                    "[" | "comment" => {}
                    // 空き (`[,]` は空の配列 `[]` と違う) は印で表す。
                    "," => parts.push(current.take().unwrap_or_else(|| "<hole>".to_owned())),
                    "]" => {
                        if let Some(element) = current.take() {
                            parts.push(element);
                        }
                    }
                    "assignment_pattern" => {
                        let shape = pattern_shape(child.child_by_field_name("left")?, source)?;
                        current = Some(format!(
                            "{}?",
                            defaulted(shape, child.child_by_field_name("right"), source)?
                        ));
                    }
                    "rest_pattern" => current = Some("...".to_owned()),
                    _ => current = Some(pattern_shape(child, source)?),
                }
            }
            Some(format!("[ {} ]", parts.join(", ")))
        }
        // 束縛先の名前は型を決めない。
        "identifier" | "shorthand_property_identifier_pattern" => Some("_".to_owned()),
        _ => node_text(pattern, source),
    }
}

/// 初期値のある分割代入の要素の形 (`形 = 初期値から推論する型`)。
fn defaulted(shape: String, value: Option<Node<'_>>, source: &[u8]) -> Option<String> {
    match value {
        Some(value) => Some(format!(
            "{shape} = {}",
            inferred_type(Some(value), source, false)?
        )),
        None => Some(shape),
    }
}

/// 引数を、宣言ファイルの形 (アクセス修飾子・`readonly`・デコレータ・初期値を除き、初期値のある
/// 引数は optional) にする。分割代入は、型注釈があれば型だけが契約なので `{}`・`[]` にし、無ければ
/// 形 ([`pattern_shape`]) から型が決まるので形にする。
fn parameter_text(parameter: Node<'_>, source: &[u8]) -> Option<String> {
    if !matches!(
        parameter.kind(),
        "required_parameter" | "optional_parameter"
    ) {
        return node_text(parameter, source);
    }
    let pattern = parameter
        .child_by_field_name("pattern")
        .or_else(|| parameter.child_by_field_name("name"))?;
    let annotated = annotation(parameter, source, "type");
    let name = match (pattern.kind(), &annotated) {
        ("object_pattern", Some(_)) => "{}".to_owned(),
        ("array_pattern", Some(_)) => "[]".to_owned(),
        ("object_pattern" | "array_pattern", None) => pattern_shape(pattern, source)?,
        _ => node_text(pattern, source)?,
    };
    let value = parameter.child_by_field_name("value");
    let optional = parameter.kind() == "optional_parameter" || value.is_some();
    let ty = match annotated {
        Some(ty) => ty,
        None => inferred_type(value, source, false)?,
    };
    Some(format!("{name}{}: {ty}", if optional { "?" } else { "" }))
}

/// 宣言の引数 (`parameters`) の並び。
fn parameters_text(declaration: Node<'_>, source: &[u8]) -> Option<String> {
    let Some(parameters) = declaration.child_by_field_name("parameters") else {
        return Some(String::new());
    };
    let mut cursor = parameters.walk();
    let texts: Option<Vec<String>> = parameters
        .named_children(&mut cursor)
        .filter(|parameter| parameter.kind() != "comment")
        .map(|parameter| parameter_text(parameter, source))
        .collect();
    Some(texts?.join(", "))
}

/// 実装のあるメソッドの宣言 `method` の、宣言ファイルの形の見出し (`prefix` の修飾子・名前・型の
/// 引数・引数 ([`parameter_text`])・戻り値の型。本体は入れない)。`async` とジェネレータは型注釈の
/// 無い戻り値の型を変えるので入れる。
fn method_header(method: Node<'_>, source: &[u8], prefix: &str, name: &str) -> Option<String> {
    let mut text = prefix.to_owned();
    let mut optional = false;
    let mut cursor = method.walk();
    for child in method.children(&mut cursor) {
        match child.kind() {
            "async" => text.push_str("async "),
            "*" => text.push_str("* "),
            "?" => optional = true,
            _ => {}
        }
    }
    text.push_str(name);
    if optional {
        text.push('?');
    }
    if let Some(type_parameters) = method.child_by_field_name("type_parameters") {
        text.push_str(&node_text(type_parameters, source)?);
    }
    text.push('(');
    text.push_str(&parameters_text(method, source)?);
    text.push(')');
    if let Some(ty) = annotation(method, source, "return_type") {
        text.push_str(": ");
        text.push_str(&ty);
    }
    Some(text)
}

/// クラスの公開のメンバーの項目。キーは (static か, 名前, 同じ名前の中の順)。値は (字面,
/// optional のフィールドか)。
type Members = BTreeMap<(bool, String, usize), (String, bool)>;

/// TypeScript のクラスの公開の形。
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ClassShape {
    header: String,
    /// コンストラクタの可視性とシグネチャ (宣言順)。引数の無い暗黙のものと同じなら空。
    constructor: Vec<String>,
    members: Members,
}

impl ClassShape {
    /// API の signature (`見出し { コンストラクタ ; メンバー }`)。
    fn signature(&self) -> String {
        let entries: Vec<&str> = self
            .constructor
            .iter()
            .map(String::as_str)
            .chain(self.members.values().map(|(text, _)| text.as_str()))
            .collect();
        if entries.is_empty() {
            format!("{} {{ }}", self.header)
        } else {
            format!("{} {{ {} }}", self.header, entries.join(" ; "))
        }
    }
}

/// クラス・enum の宣言 `declaration` の見出し (本体の前まで。デコレータを除き、外側の `export`・
/// `export default` を前に付ける。`declare` は宣言ファイルの形を変えないので付けない)。
fn header(declaration: Node<'_>, body: Node<'_>, source: &[u8]) -> Option<String> {
    let mut cursor = declaration.walk();
    let start = declaration
        .children(&mut cursor)
        .find(|child| !matches!(child.kind(), "decorator" | "comment"))?
        .start_byte();
    let text = range_text(declaration, source, start, body.start_byte())?;
    let mut outer = declaration.parent();
    while let Some(parent) = outer.filter(|parent| parent.kind() == "ambient_declaration") {
        outer = parent.parent();
    }
    let prefix = match outer.filter(|parent| parent.kind() == "export_statement") {
        Some(statement)
            if statement
                .children(&mut statement.walk())
                .any(|child| child.kind() == "default") =>
        {
            "export default "
        }
        Some(_) => "export ",
        None => "",
    };
    Some(format!("{prefix}{text}"))
}

/// クラスの宣言 `declaration` (`class_declaration`・`abstract_class_declaration`) の公開の形。
fn class_shape(declaration: Node<'_>, source: &[u8]) -> Option<ClassShape> {
    let body = declaration.child_by_field_name("body")?;
    let header = header(declaration, body, source)?;
    let has_extends = declaration
        .named_children(&mut declaration.walk())
        .any(|child| {
            child.kind() == "class_heritage"
                && child
                    .named_children(&mut child.walk())
                    .any(|clause| clause.kind() == "extends_clause")
        });
    let mut cursor = body.walk();
    let elements: Vec<Node<'_>> = body
        .named_children(&mut cursor)
        .filter(|element| element.kind() != "comment")
        .collect();
    let mut members = Members::new();
    let mut constructor_implementation = None;
    let mut constructor_overloads = Vec::new();
    // 実装のあるメソッドの名前 (static か)。同じ名前の実装の無い宣言はそのオーバーロード。
    let implemented: Vec<(bool, String)> = elements
        .iter()
        .filter(|element| element.kind() == "method_definition")
        .filter_map(|element| {
            Some((
                Modifiers::of(*element, source).is_static,
                member_name(*element, source)?,
            ))
        })
        .collect();
    // static とインスタンスに同じ名前のメソッドがあると、メソッドのシンボルの qualname
    // (`Client.send`) が重なって比べられないので、その宣言 (オーバーロードか実装の見出し) を
    // クラスの形に入れる。
    let mut method_kinds: HashMap<String, HashSet<bool>> = HashMap::new();
    for element in &elements {
        if element.kind() == "method_definition"
            && !is_accessor(*element)
            && let Some(name) = member_name(*element, source)
        {
            method_kinds
                .entry(name)
                .or_default()
                .insert(Modifiers::of(*element, source).is_static);
        }
    }
    let mut order: BTreeMap<(bool, String), usize> = BTreeMap::new();
    let mut next_slot = |is_static: bool, name: &str| {
        let slot = order.entry((is_static, name.to_owned())).or_insert(0);
        *slot += 1;
        *slot
    };
    for element in elements {
        let modifiers = Modifiers::of(element, source);
        let name = member_name(element, source);
        if name.as_deref() == Some("constructor")
            && matches!(element.kind(), "method_definition" | "method_signature")
        {
            if element.kind() == "method_definition" {
                constructor_implementation = Some(element);
            } else {
                constructor_overloads.push(element);
            }
            continue;
        }
        if modifiers.private() || is_hash_private(element) {
            continue;
        }
        let prefix = modifiers.prefix();
        match element.kind() {
            "public_field_definition" => {
                let name = name?;
                let ty = match annotation(element, source, "type") {
                    Some(ty) => ty,
                    None => inferred_type(
                        element.child_by_field_name("value"),
                        source,
                        modifiers.readonly,
                    )?,
                };
                let optional = if modifiers.optional { "?" } else { "" };
                members.insert(
                    (modifiers.is_static, name.clone(), 0),
                    (
                        format!("{prefix}{name}{optional}: {ty}"),
                        modifiers.optional,
                    ),
                );
            }
            "method_definition" => {
                // アクセサは型の読み書きを持つ 1 つのプロパティ。ほかのメソッドは別のシンボル。
                let accessor = element
                    .children(&mut element.walk())
                    .find(|child| matches!(child.kind(), "get" | "set"))
                    .map(|child| child.kind());
                let Some(accessor) = accessor else {
                    let name = name?;
                    if method_kinds.get(&name).is_some_and(|kinds| kinds.len() > 1) {
                        let overloads = overload_declarations(element, source);
                        let text = if overloads.is_empty() {
                            method_header(element, source, &prefix, &name)?
                        } else {
                            overloads_signature(&overloads, source)?
                        };
                        members.insert((modifiers.is_static, name, 2), (text, false));
                    }
                    continue;
                };
                let name = name?;
                let text = if accessor == "get" {
                    match annotation(element, source, "return_type") {
                        Some(ty) => format!("{prefix}get {name}(): {ty}"),
                        None => format!("{prefix}get {name}()"),
                    }
                } else {
                    format!("{prefix}set {name}({})", parameters_text(element, source)?)
                };
                let slot = usize::from(accessor == "set");
                members.insert((modifiers.is_static, name, slot), (text, false));
            }
            // 実装の無いメソッドの宣言: abstract と、実装の無い (ambient の) 宣言。実装のある
            // メソッドのオーバーロードは、そのメソッドのシンボルの signature に入る。
            "abstract_method_signature" | "method_signature" => {
                let name = name?;
                if element.kind() == "method_signature"
                    && implemented.contains(&(modifiers.is_static, name.clone()))
                {
                    continue;
                }
                let slot = next_slot(modifiers.is_static, &name) + 1;
                let text = node_text(element, source)?;
                let text = text.strip_suffix(" ;").unwrap_or(&text).to_owned();
                members.insert((modifiers.is_static, name, slot), (text, false));
            }
            "index_signature" => {
                let slot = next_slot(modifiers.is_static, "[");
                let text = node_text(element, source)?;
                let text = text.strip_suffix(" ;").unwrap_or(&text).to_owned();
                members.insert((modifiers.is_static, "[".to_owned(), slot), (text, false));
            }
            _ => {}
        }
    }
    // パラメータプロパティは公開のフィールド。
    if let Some(implementation) = constructor_implementation
        && let Some(parameters) = implementation.child_by_field_name("parameters")
    {
        let mut cursor = parameters.walk();
        for parameter in parameters.named_children(&mut cursor) {
            if !matches!(
                parameter.kind(),
                "required_parameter" | "optional_parameter"
            ) {
                continue;
            }
            let modifiers = Modifiers::of(parameter, source);
            if !(modifiers.property_marker || modifiers.readonly) || modifiers.private() {
                continue;
            }
            let name = node_text(
                parameter
                    .child_by_field_name("pattern")
                    .or_else(|| parameter.child_by_field_name("name"))?,
                source,
            )?;
            let ty = match annotation(parameter, source, "type") {
                Some(ty) => ty,
                None => inferred_type(parameter.child_by_field_name("value"), source, false)?,
            };
            let optional = parameter.kind() == "optional_parameter";
            members.insert(
                (false, name.clone(), 0),
                (
                    format!(
                        "{}{name}{}: {ty}",
                        modifiers.prefix(),
                        if optional { "?" } else { "" }
                    ),
                    optional,
                ),
            );
        }
    }
    let constructor = constructor_entries(
        constructor_implementation,
        &constructor_overloads,
        has_extends,
        source,
    )?;
    Some(ClassShape {
        header,
        constructor,
        members,
    })
}

/// コンストラクタの可視性とシグネチャ。オーバーロードがあればその宣言 (実装は宣言ファイルに
/// 出ない)、無ければ実装。`private` なら引数は外から見えない。`extends` の無いクラスの、引数の
/// 無い公開のコンストラクタは暗黙のものと同じなので空にする。
fn constructor_entries(
    implementation: Option<Node<'_>>,
    overloads: &[Node<'_>],
    has_extends: bool,
    source: &[u8],
) -> Option<Vec<String>> {
    let declarations: Vec<Node<'_>> = if overloads.is_empty() {
        implementation.into_iter().collect()
    } else {
        overloads.to_vec()
    };
    let Some(&first) = declarations.first() else {
        return Some(Vec::new());
    };
    let visibility = Modifiers::of(implementation.unwrap_or(first), source).accessibility;
    if visibility.as_deref() == Some("private") {
        return Some(vec!["private constructor".to_owned()]);
    }
    let prefix = visibility
        .map(|text| format!("{text} "))
        .unwrap_or_default();
    let entries: Option<Vec<String>> = declarations
        .iter()
        .map(|declaration| {
            Some(format!(
                "{prefix}constructor({})",
                parameters_text(*declaration, source)?
            ))
        })
        .collect();
    let entries = entries?;
    if !has_extends && entries == ["constructor()"] {
        return Some(Vec::new());
    }
    Some(entries)
}

/// 範囲 `range` の symbol の宣言 (`kinds` のどれかで、名前が `name` のもの)。
fn declaration_for<'tree>(
    root: Node<'tree>,
    range: &crate::models::location::Range,
    name: &str,
    kinds: &[&str],
    source: &[u8],
) -> Option<Node<'tree>> {
    let start = tree_sitter::Point {
        row: range.start.line,
        column: range.start.column,
    };
    let end = tree_sitter::Point {
        row: range.end.line,
        column: range.end.column,
    };
    let mut node = root.descendant_for_point_range(start, end)?;
    loop {
        if kinds.contains(&node.kind()) && name_of(node, source) == Some(name) {
            return Some(node);
        }
        node = node.parent()?;
    }
}

const CLASS_KINDS: &[&str] = &["class_declaration", "abstract_class_declaration"];

/// クラスのシンボル `symbol` の API の signature。宣言を読めなければ None。
pub(crate) fn class_signature(
    symbol: &crate::models::symbol::Symbol,
    root: Node<'_>,
    source: &[u8],
) -> Option<String> {
    if root.has_error() {
        return None;
    }
    let declaration =
        declaration_for(root, &symbol.range, bare(&symbol.name), CLASS_KINDS, source)?;
    Some(class_shape(declaration, source)?.signature())
}

/// ファイルの中の、名前が `qualname` の末尾の名前のクラスの宣言 (ただ 1 つのときだけ)。
fn unique_class<'tree>(root: Node<'tree>, source: &[u8], qualname: &str) -> Option<Node<'tree>> {
    let name = bare(qualname);
    let mut found = None;
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if CLASS_KINDS.contains(&node.kind()) && name_of(node, source) == Some(name) {
            if found.is_some() {
                return None;
            }
            found = Some(node);
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    found
}

/// ファイルの中の、名前が `qualname` のクラスの公開の形 (ただ 1 つのクラスのときだけ)。
pub(crate) fn unique_class_shape(
    root: Node<'_>,
    source: &[u8],
    qualname: &str,
) -> Option<ClassShape> {
    if root.has_error() {
        return None;
    }
    class_shape(unique_class(root, source, qualname)?, source)
}

/// クラスの公開の形の変化。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ClassChange {
    /// メンバーとコンストラクタの契約が同じ (見出しだけの変更)。
    HeaderOnly,
    /// 見出しとコンストラクタと既存のメンバーが同じで、optional のフィールドを足しただけ。
    Compatible,
    /// ほかの変更 (削除・型や修飾子の変更・必須のメンバーやメソッドの宣言の追加・コンストラクタの
    /// 変更)。クラスの型に代入するオブジェクトや `implements`・継承を壊し得る。
    Breaking,
}

/// クラスの旧新の公開の形を比べる。
pub(crate) fn classify_class(old: &ClassShape, new: &ClassShape) -> ClassChange {
    if old.constructor == new.constructor && old.members == new.members {
        return ClassChange::HeaderOnly;
    }
    if old.header != new.header || old.constructor != new.constructor {
        return ClassChange::Breaking;
    }
    if old
        .members
        .iter()
        .any(|(key, member)| new.members.get(key) != Some(member))
    {
        return ClassChange::Breaking;
    }
    if new
        .members
        .iter()
        .filter(|(key, _)| !old.members.contains_key(*key))
        .all(|(_, (_, optional))| *optional)
    {
        ClassChange::Compatible
    } else {
        ClassChange::Breaking
    }
}

/// enum のメンバーの値。
enum EnumValue {
    Number(f64),
    Text(String),
}

impl EnumValue {
    fn render(&self) -> String {
        match self {
            EnumValue::Number(number) if number.fract() == 0.0 && number.abs() < 1e15 => {
                format!("{}", *number as i64)
            }
            EnumValue::Number(number) => number.to_string(),
            EnumValue::Text(text) => text.clone(),
        }
    }
}

/// 数のリテラルの値 (`_` の区切り・16 進・8 進・2 進を含む)。
fn number_value(text: &str) -> Option<f64> {
    let cleaned: String = text.chars().filter(|c| *c != '_').collect();
    let lower = cleaned.to_ascii_lowercase();
    for (prefix, radix) in [("0x", 16), ("0o", 8), ("0b", 2)] {
        if let Some(digits) = lower.strip_prefix(prefix) {
            return i128::from_str_radix(digits, radix)
                .ok()
                .map(|value| value as f64);
        }
    }
    cleaned.parse::<f64>().ok()
}

/// enum のメンバーの初期値。数のリテラル (負の数を含む) は数、ほかは字面 ([`initializer_text`])。
fn initializer_value(
    value: Node<'_>,
    source: &[u8],
    enum_name: &str,
    values: &HashMap<String, String>,
) -> Option<EnumValue> {
    match value.kind() {
        "number" => Some(match number_value(value.utf8_text(source).ok()?) {
            Some(number) => EnumValue::Number(number),
            None => EnumValue::Text(node_text(value, source)?),
        }),
        "unary_expression"
            if value
                .child_by_field_name("operator")
                .is_some_and(|operator| operator.kind() == "-") =>
        {
            let argument = value.child_by_field_name("argument")?;
            match initializer_value(argument, source, enum_name, values)? {
                EnumValue::Number(number) => Some(EnumValue::Number(-number)),
                EnumValue::Text(_) => Some(EnumValue::Text(initializer_text(
                    value, source, enum_name, values,
                )?)),
            }
        }
        _ => Some(EnumValue::Text(initializer_text(
            value, source, enum_name, values,
        )?)),
    }
}

/// enum のメンバーの初期値の字面 (トークンを空白でつなぐ)。同じ enum の前のメンバーの参照
/// (`A`・`Mode.A`) は、その値に置き換える (`stripInternal` で消えるメンバーの値の変化も、参照する
/// メンバーの値の変化として出す)。
fn initializer_text(
    value: Node<'_>,
    source: &[u8],
    enum_name: &str,
    values: &HashMap<String, String>,
) -> Option<String> {
    let mut parts = Vec::new();
    let mut stack = vec![value];
    while let Some(node) = stack.pop() {
        let referenced = match node.kind() {
            "identifier" => node.utf8_text(source).ok(),
            "member_expression" => node
                .child_by_field_name("object")
                .filter(|object| {
                    object.kind() == "identifier"
                        && object.utf8_text(source).ok() == Some(enum_name)
                })
                .and_then(|_| node.child_by_field_name("property"))
                .and_then(|property| property.utf8_text(source).ok()),
            _ => None,
        };
        if let Some(value) = referenced.and_then(|name| values.get(name)) {
            parts.push(format!("( {value} )"));
            continue;
        }
        if node.kind() == "comment" {
            continue;
        }
        if node.child_count() == 0 || matches!(node.kind(), "string" | "template_string") {
            parts.push(node.utf8_text(source).ok()?.to_owned());
            continue;
        }
        let mut cursor = node.walk();
        let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
        stack.extend(children.into_iter().rev());
    }
    Some(parts.join(" "))
}

/// enum の宣言 `declaration` のメンバーと値 (宣言順)。値は全部のメンバーから求め (暗黙の値は前の
/// メンバーの次)、`strip` なら `stripInternal` で宣言ファイルから消えるメンバーを除く。
fn enum_members(declaration: Node<'_>, source: &[u8], strip: bool) -> Option<Vec<String>> {
    let body = declaration.child_by_field_name("body")?;
    let enum_name = name_of(declaration, source)?;
    let mut members = Vec::new();
    let mut values: HashMap<String, String> = HashMap::new();
    let mut previous: Option<EnumValue> = None;
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor) {
        let (name_node, value) = match member.kind() {
            "comment" => continue,
            "enum_assignment" => (
                member.child_by_field_name("name")?,
                Some(initializer_value(
                    member.child_by_field_name("value")?,
                    source,
                    enum_name,
                    &values,
                )?),
            ),
            _ => (member, None),
        };
        let name = if name_node.kind() == "string" {
            let text = name_node.utf8_text(source).ok()?;
            text.get(1..text.len().checked_sub(1)?)?.to_owned()
        } else {
            node_text(name_node, source)?
        };
        let value = match (value, &previous) {
            (Some(value), _) => value,
            (None, None) => EnumValue::Number(0.0),
            (None, Some(EnumValue::Number(number))) => EnumValue::Number(number + 1.0),
            (None, Some(EnumValue::Text(text))) => EnumValue::Text(format!("( {text} ) + 1")),
        };
        let rendered = value.render();
        if !(strip && super::ts_strip_internal::is_stripped_member(member, source)) {
            members.push(format!("{name} = {rendered}"));
        }
        values.insert(name, rendered);
        previous = Some(value);
    }
    Some(members)
}

/// enum のシンボル `symbol` の API の signature (`見出し { A = 0 , B = 1 }`)。`original` は
/// `stripInternal` を当てる前のファイル (当てるときだけ。メンバーの値はこちらから求める)。
pub(crate) fn enum_signature(
    symbol: &crate::models::symbol::Symbol,
    root: Node<'_>,
    source: &[u8],
    original: Option<(Node<'_>, &[u8])>,
) -> Option<String> {
    if root.has_error() {
        return None;
    }
    let name = bare(&symbol.name);
    let declaration = declaration_for(root, &symbol.range, name, &["enum_declaration"], source)?;
    let header = header(
        declaration,
        declaration.child_by_field_name("body")?,
        source,
    )?;
    let members = match original {
        Some((original_root, original_source)) => {
            // 空白にしただけで位置は同じなので、同じ範囲の宣言を元のファイルで引く。
            let mut node = original_root
                .descendant_for_byte_range(declaration.start_byte(), declaration.end_byte())?;
            while !(node.kind() == "enum_declaration"
                && node.start_byte() == declaration.start_byte())
            {
                node = node.parent()?;
            }
            enum_members(node, original_source, true)?
        }
        None => enum_members(declaration, source, false)?,
    };
    if members.is_empty() {
        Some(format!("{header} {{ }}"))
    } else {
        Some(format!("{header} {{ {} }}", members.join(" , ")))
    }
}

/// enum の旧新の signature で、メンバーの部分が変わったか (見出しには `{` が入らないので、最初の
/// ` { ` で分ける)。分けられなければ変わったとみなす。
pub(crate) fn enum_members_changed(old: &str, new: &str) -> bool {
    match (old.split_once(" { "), new.split_once(" { ")) {
        (Some((_, old_members)), Some((_, new_members))) => old_members != new_members,
        _ => true,
    }
}

/// 実装の宣言 `implementation` (`function_declaration`・`method_definition`) と同じ本体にある、同じ
/// 名前 (メソッドは static かも同じ) のオーバーロードの宣言 (宣言順)。宣言ファイルに出るのは
/// こちらで、実装の宣言は出ない。
pub(crate) fn overload_declarations<'tree>(
    implementation: Node<'tree>,
    source: &[u8],
) -> Vec<Node<'tree>> {
    let (signature_kind, anchor) = match implementation.kind() {
        "function_declaration" => (
            "function_signature",
            implementation
                .parent()
                .filter(|parent| parent.kind() == "export_statement")
                .unwrap_or(implementation),
        ),
        "method_definition" => ("method_signature", implementation),
        _ => return Vec::new(),
    };
    let (Some(body), Some(name)) = (anchor.parent(), member_name(implementation, source)) else {
        return Vec::new();
    };
    let is_static = Modifiers::of(implementation, source).is_static;
    let mut cursor = body.walk();
    body.named_children(&mut cursor)
        .map(declaration_of)
        .filter(|declaration| {
            declaration.kind() == signature_kind
                && member_name(*declaration, source).as_deref() == Some(name.as_str())
                && (signature_kind == "function_signature"
                    || Modifiers::of(*declaration, source).is_static == is_static)
        })
        .collect()
}

/// オーバーロードの宣言の並びを API の signature にする (宣言順に ` ; ` でつなぐ)。
pub(crate) fn overloads_signature(overloads: &[Node<'_>], source: &[u8]) -> Option<String> {
    let texts: Option<Vec<String>> = overloads
        .iter()
        .map(|overload| {
            let text = super::signature::normalize_signature_whitespace(
                overload.utf8_text(source).ok()?.as_bytes(),
            );
            Some(text.trim_end_matches(';').trim_end().to_owned())
        })
        .collect();
    Some(texts?.join(" ; "))
}

/// ファイルで、名前が `qualname` (`f`・`Client.send`) の関数・メソッドの実装にオーバーロードの宣言が
/// あるか。実装を引けなければ None。
pub(crate) fn has_overloads(root: Node<'_>, source: &[u8], qualname: &str) -> Option<bool> {
    let implementations: Vec<Node<'_>> = match qualname.rsplit_once('.') {
        Some((owner, method)) => {
            let class = unique_class(root, source, owner)?;
            let body = class.child_by_field_name("body")?;
            let mut cursor = body.walk();
            body.named_children(&mut cursor)
                .filter(|element| {
                    element.kind() == "method_definition"
                        && member_name(*element, source).as_deref() == Some(method)
                })
                .collect()
        }
        None => {
            let mut found = Vec::new();
            let mut stack = vec![root];
            while let Some(node) = stack.pop() {
                if node.kind() == "function_declaration" && name_of(node, source) == Some(qualname)
                {
                    found.push(node);
                }
                let mut cursor = node.walk();
                stack.extend(node.named_children(&mut cursor));
            }
            found
        }
    };
    if implementations.is_empty() {
        return None;
    }
    Some(
        implementations
            .iter()
            .any(|implementation| !overload_declarations(*implementation, source).is_empty()),
    )
}

/// メソッドの宣言 `method` がアクセサ (`get`・`set`) か。
fn is_accessor(method: Node<'_>) -> bool {
    method
        .children(&mut method.walk())
        .any(|child| matches!(child.kind(), "get" | "set"))
}

/// TypeScript のメソッドのシンボルが、名前のあるクラスの宣言のアクセサ (`get`・`set`) か。
/// アクセサはそのクラスの公開の形に入るので、別のシンボルとして API に数えない (二重に報告
/// しない)。名前の無いクラス (`export default class { … }`・クラス式) は形を作らないので外さない。
pub(crate) fn is_accessor_symbol(symbol: &crate::models::symbol::Symbol, root: Node<'_>) -> bool {
    let start = tree_sitter::Point {
        row: symbol.range.start.line,
        column: symbol.range.start.column,
    };
    let Some(mut node) = root.descendant_for_point_range(start, start) else {
        return false;
    };
    while node.kind() != "method_definition" {
        let Some(parent) = node.parent() else {
            return false;
        };
        node = parent;
    }
    is_accessor(node)
        && node
            .parent()
            .filter(|body| body.kind() == "class_body")
            .and_then(|body| body.parent())
            .is_some_and(|class| {
                CLASS_KINDS.contains(&class.kind()) && class.child_by_field_name("name").is_some()
            })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::parser;
    use crate::language::LangId;

    fn shape(source: &str) -> ClassShape {
        let tree = parser::parse_source(source.as_bytes(), LangId::Typescript).unwrap();
        unique_class_shape(tree.root_node(), source.as_bytes(), "Client").expect("class")
    }

    fn class_change(old: &str, new: &str) -> ClassChange {
        classify_class(&shape(old), &shape(new))
    }

    #[test]
    fn class_shape_lists_the_public_contract() {
        let source = "@sealed\nexport class Client<T extends { a: string }> extends Base implements Api {\n  token: string = \"\";\n  static count = 0;\n  readonly kind = \"client\";\n  protected cache?: Map<string, T>;\n  private secret = 1;\n  #hidden = 2;\n  [key: string]: unknown;\n  constructor(public retries: number, private readonly id: string, protected label = \"x\") {\n    super();\n  }\n  get value(): number { return 1; }\n  set value(v: number) {}\n  send(): void {}\n}\n";
        assert_eq!(
            shape(source).signature(),
            "export class Client < T extends { a : string } > extends Base implements Api { constructor(retries: number, id: string, label?: string) ; [ key : string ] : unknown ; protected cache?: Map < string , T > ; readonly kind: \"client\" ; protected label: string ; retries: number ; token: string ; get value(): number ; set value(v: number) ; static count: number }"
        );
    }

    #[test]
    fn classes_compare_members_and_constructors() {
        let base = "export class Client {\n  token: string = \"\";\n  constructor(public retries: number) {}\n}\n";
        // 本体・初期値・private のメンバー・整形だけの変更は同じ
        assert_eq!(
            class_change(
                base,
                "export class Client {\n  // comment\n  token: string = \"abc\";\n  private extra = 1;\n  constructor(\n    public retries: number,\n  ) {\n    console.log(1);\n  }\n}\n"
            ),
            ClassChange::HeaderOnly
        );
        // フィールド・パラメータプロパティの削除と型の変更、コンストラクタの引数の変更
        for new in [
            "export class Client {\n  constructor(public retries: number) {}\n}\n",
            "export class Client {\n  token: string = \"\";\n  constructor(retries: number) {}\n}\n",
            "export class Client {\n  token: number = 0;\n  constructor(public retries: number) {}\n}\n",
            "export class Client {\n  token: string = \"\";\n  constructor(public retries: number, extra: string) {}\n}\n",
            "export class Client {\n  token: string = \"\";\n  protected constructor(public retries: number) {}\n}\n",
            "export class Client {\n  token: string = \"\";\n  required: number = 0;\n  constructor(public retries: number) {}\n}\n",
        ] {
            assert_eq!(class_change(base, new), ClassChange::Breaking, "{new}");
        }
        // optional のフィールドの追加だけは互換
        assert_eq!(
            class_change(
                base,
                "export class Client {\n  token: string = \"\";\n  note?: string;\n  constructor(public retries: number) {}\n}\n"
            ),
            ClassChange::Compatible
        );
        // 見出しだけの変更
        assert_eq!(
            class_change(
                base,
                "export abstract class Client {\n  token: string = \"\";\n  constructor(public retries: number) {}\n}\n"
            ),
            ClassChange::HeaderOnly
        );
    }

    #[test]
    fn constructors_follow_the_declaration_file() {
        // 暗黙のコンストラクタと、extends の無いクラスの引数の無い公開のコンストラクタは同じ
        assert_eq!(
            class_change(
                "export class Client {}\n",
                "export class Client {\n  constructor() {}\n}\n"
            ),
            ClassChange::HeaderOnly
        );
        // private のコンストラクタの引数は外から見えない
        assert_eq!(
            class_change(
                "export class Client {\n  private constructor(a: string) {}\n}\n",
                "export class Client {\n  private constructor(a: number) {}\n}\n"
            ),
            ClassChange::HeaderOnly
        );
        // オーバーロードがあれば実装の引数は比べない
        assert_eq!(
            class_change(
                "export class Client {\n  constructor(a: string);\n  constructor(a: any) {}\n}\n",
                "export class Client {\n  constructor(a: string);\n  constructor(a: unknown) {}\n}\n"
            ),
            ClassChange::HeaderOnly
        );
        assert_eq!(
            class_change(
                "export class Client {\n  constructor(a: string);\n  constructor(a: number);\n  constructor(a: any) {}\n}\n",
                "export class Client {\n  constructor(a: string);\n  constructor(a: any) {}\n}\n"
            ),
            ClassChange::Breaking
        );
    }

    fn enum_sig(source: &str, strip: bool) -> String {
        let tree = parser::parse_source(source.as_bytes(), LangId::Typescript).unwrap();
        let root = tree.root_node();
        let symbols =
            crate::engine::symbols::extract_symbols(root, source.as_bytes(), LangId::Typescript)
                .unwrap();
        let symbol = symbols
            .iter()
            .find(|symbol| symbol.name == "Mode")
            .expect("Mode");
        if strip {
            let published =
                super::super::ts_strip_internal::published_source(root, source.as_bytes())
                    .unwrap_or_else(|| source.as_bytes().to_vec());
            let published_tree = parser::parse_source(&published, LangId::Typescript).unwrap();
            enum_signature(
                symbol,
                published_tree.root_node(),
                &published,
                Some((root, source.as_bytes())),
            )
            .expect("signature")
        } else {
            enum_signature(symbol, root, source.as_bytes(), None).expect("signature")
        }
    }

    #[test]
    fn enums_list_members_with_values() {
        assert_eq!(
            enum_sig(
                "export enum Mode {\n  A,\n  B = 5,\n  C,\n  'D' = \"d\",\n  E = 0x10,\n  F,\n}\n",
                false
            ),
            "export enum Mode { A = 0 , B = 5 , C = 6 , D = \"d\" , E = 16 , F = 17 }"
        );
        // 暗黙の値と同じ明示の値は同じ
        assert_eq!(
            enum_sig("export enum Mode {\n  A,\n  B,\n}\n", false),
            enum_sig("export enum Mode {\n  A = 0,\n  B = 1,\n}\n", false)
        );
        // `stripInternal` で消えるメンバーの後の暗黙の値は、消える前の宣言から求める
        assert_eq!(
            enum_sig(
                "export enum Mode {\n  A,\n  /** @internal */\n  B,\n  C,\n}\n",
                true
            ),
            "export enum Mode { A = 0 , C = 2 }"
        );
        assert!(enum_members_changed(
            "export enum Mode { A = 0 , B = 1 }",
            "export enum Mode { A = 0 }"
        ));
        assert!(!enum_members_changed(
            "export enum Mode { A = 0 }",
            "export const enum Mode { A = 0 }"
        ));
    }

    #[test]
    fn overloads_replace_the_implementation_signature() {
        let source = "export function f(x: string): string;\nexport function f(x: number): number;\nexport function f(x: any): any {\n  return x;\n}\nexport class Client {\n  send(a: string): void;\n  send(a: any): void {}\n  static send(): void {}\n  plain(): void {}\n}\n";
        let tree = parser::parse_source(source.as_bytes(), LangId::Typescript).unwrap();
        let root = tree.root_node();
        assert_eq!(has_overloads(root, source.as_bytes(), "f"), Some(true));
        assert_eq!(
            has_overloads(root, source.as_bytes(), "Client.send"),
            Some(true)
        );
        assert_eq!(
            has_overloads(root, source.as_bytes(), "Client.plain"),
            Some(false)
        );
        assert_eq!(has_overloads(root, source.as_bytes(), "missing"), None);
        let implementation = {
            let mut stack = vec![root];
            let mut found = None;
            while let Some(node) = stack.pop() {
                if node.kind() == "function_declaration" {
                    found = Some(node);
                }
                let mut cursor = node.walk();
                stack.extend(node.named_children(&mut cursor));
            }
            found.unwrap()
        };
        let overloads = overload_declarations(implementation, source.as_bytes());
        assert_eq!(
            overloads_signature(&overloads, source.as_bytes()).as_deref(),
            Some("function f(x: string): string ; function f(x: number): number")
        );
    }

    /// 1 回目のレビューの反例。
    #[test]
    fn review_counterexamples_are_compared() {
        // bigint のリテラルは number ではない
        assert_eq!(
            class_change(
                "export class Client {\n  value = 1n;\n}\n",
                "export class Client {\n  value = 1;\n}\n"
            ),
            ClassChange::Breaking
        );
        // 型注釈の無い分割代入の引数は、取り出す名前が型を決める
        assert_eq!(
            class_change(
                "export class Client {\n  constructor({ x }) {}\n}\n",
                "export class Client {\n  constructor({ y }) {}\n}\n"
            ),
            ClassChange::Breaking
        );
        // 型注釈があれば名前は契約でない
        assert_eq!(
            class_change(
                "export class Client {\n  constructor({ x }: Options) {}\n}\n",
                "export class Client {\n  constructor({ y }: Options) {}\n}\n"
            ),
            ClassChange::HeaderOnly
        );
        // static とインスタンスに同じ名前のメソッドがあれば、その宣言をクラスの形に入れる
        assert_eq!(
            class_change(
                "export class Client {\n  send(a: string): void;\n  send(a: number): void;\n  send(a: any): void {}\n  static send(): void {}\n}\n",
                "export class Client {\n  send(a: string): void;\n  send(a: any): void {}\n  static send(): void {}\n}\n"
            ),
            ClassChange::Breaking
        );
        assert_eq!(
            class_change(
                "export class Client {\n  send(a: string): void {}\n  static send(): void {}\n}\n",
                "export class Client {\n  send(a: number): void {}\n  static send(): void {}\n}\n"
            ),
            ClassChange::Breaking
        );
    }

    #[test]
    fn enum_members_referencing_internal_members_follow_their_values() {
        assert_eq!(
            enum_sig(
                "export enum Mode {\n  /** @internal */\n  A = 1,\n  B = A,\n  C = Mode.A | 2,\n}\n",
                true
            ),
            "export enum Mode { B = ( 1 ) , C = ( 1 ) | 2 }"
        );
        assert_ne!(
            enum_sig(
                "export enum Mode {\n  /** @internal */\n  A = 1,\n  B = A,\n}\n",
                true
            ),
            enum_sig(
                "export enum Mode {\n  /** @internal */\n  A = 2,\n  B = A,\n}\n",
                true
            )
        );
    }

    /// 2 回目のレビューの反例: 分割代入のローカルな変数の名前は契約でない。
    #[test]
    fn destructured_binding_names_are_not_contracts() {
        assert_eq!(
            class_change(
                "export class Client {\n  constructor({ x: a, y = 1 }) {}\n}\n",
                "export class Client {\n  constructor({ x: b, y: c = 2 }) {}\n}\n"
            ),
            ClassChange::HeaderOnly
        );
        assert_eq!(
            class_change(
                "export class Client {\n  send({ x: a }: T): void {}\n  static send(): void {}\n}\n",
                "export class Client {\n  send({ x: b }: T): void {}\n  static send(): void {}\n}\n"
            ),
            ClassChange::HeaderOnly
        );
        // 取り出すプロパティの名前は契約
        assert_eq!(
            class_change(
                "export class Client {\n  constructor({ x: a }) {}\n}\n",
                "export class Client {\n  constructor({ z: a }) {}\n}\n"
            ),
            ClassChange::Breaking
        );
        // 初期値の有無は optional かどうか
        assert_eq!(
            class_change(
                "export class Client {\n  constructor([a, b]) {}\n}\n",
                "export class Client {\n  constructor([a, b = 1]) {}\n}\n"
            ),
            ClassChange::Breaking
        );
        // 3 回目のレビューの反例: 省略形と改名は同じ、初期値の型と配列の空きは形
        assert_eq!(
            class_change(
                "export class Client {\n  constructor({ y, w = 1 }) {}\n}\n",
                "export class Client {\n  constructor({ y: z, w: v = 2 }) {}\n}\n"
            ),
            ClassChange::HeaderOnly
        );
        assert_eq!(
            class_change(
                "export class Client {\n  constructor({ x = 1 }) {}\n}\n",
                "export class Client {\n  constructor({ x = \"a\" }) {}\n}\n"
            ),
            ClassChange::Breaking
        );
        assert_eq!(
            class_change(
                "export class Client {\n  constructor([a]) {}\n}\n",
                "export class Client {\n  constructor([, a]) {}\n}\n"
            ),
            ClassChange::Breaking
        );
        // 4 回目のレビューの反例: 空の配列と空き 1 つの配列は違う
        assert_eq!(
            class_change(
                "export class Client {\n  constructor([]) {}\n}\n",
                "export class Client {\n  constructor([,]) {}\n}\n"
            ),
            ClassChange::Breaking
        );
    }
}
