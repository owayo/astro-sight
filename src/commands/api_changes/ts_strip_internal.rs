//! TypeScript の `stripInternal` (Issue #274)。
//!
//! ファイルに適用される設定 (`extends` の先を含む実効値) が `stripInternal` を有効にすると、
//! 前のコメントに `@internal` のある宣言 (export・クラスやインタフェースのメンバー・入れ子の型の
//! メンバーなど) は宣言ファイルから消える。API の差分では、宣言ファイルを使う外の利用者から見た面
//! として、そうした宣言を公開の一覧から除き、型のメンバーはシグネチャと互換の判定から除く
//! ([`published_source`])。`@internal` の付け足しは、その宣言の削除と同じ区分になる。

use std::cell::RefCell;
use std::ops::Range;

use tree_sitter::Node;

use crate::language::LangId;
use crate::models::symbol::Symbol;

/// リポジトリの `stripInternal` の設定。TS のソースに `@internal` があるときだけ、ファイルに
/// 適用される設定を読む (設定ごとに覚える)。旧版のソースは base の版の設定で判定する (Issue #287)。
pub(crate) struct StripInternal {
    /// 今の設定 (新版のソース)。
    configs: Option<RefCell<super::ts_alias_resolution::StripInternalConfigs>>,
    /// base の版の設定を読む場所 (`dir`, 版)。[`Self::with_base`] のときだけ。
    base: Option<(String, String)>,
    /// base の版の設定 (旧版のソース)。初めて要るとき (`@internal` を含む TS のソースか、設定の
    /// 変更) に読み、`@internal` の無い差分では git を動かさない。base の tree を読めなければ None
    /// で、旧版も今の設定で判定する。
    base_configs:
        std::cell::OnceCell<Option<RefCell<super::ts_alias_resolution::StripInternalConfigs>>>,
    /// 片方の設定を決められなかったファイルの、もう片方の判定 ((旧版か, パス) → 当たるか)。
    /// [`Self::align`]。
    overrides: RefCell<std::collections::HashMap<(bool, String), bool>>,
}

impl StripInternal {
    /// 旧版も新版も今の設定で判定する。
    pub(crate) fn new(dir: &str) -> Self {
        Self {
            configs: Some(RefCell::new(
                super::ts_alias_resolution::StripInternalConfigs::new(dir),
            )),
            base: None,
            base_configs: std::cell::OnceCell::new(),
            overrides: RefCell::default(),
        }
    }

    /// 旧版のソースを base の版 `rev` の設定で、新版を今の設定で判定する。base の tree を読めな
    /// ければ、旧版も今の設定で判定する ([`Self::has_base`] が false)。
    pub(crate) fn with_base(dir: &str, rev: &str) -> Self {
        Self {
            base: Some((dir.to_owned(), rev.to_owned())),
            ..Self::new(dir)
        }
    }

    /// 設定を読まず、`@internal` を見ない (API の差分以外の用途)。
    pub(crate) fn disabled() -> Self {
        Self {
            configs: None,
            base: None,
            base_configs: std::cell::OnceCell::new(),
            overrides: RefCell::default(),
        }
    }

    /// base の版の設定 (初めて要るときに読む)。
    fn base_configs(&self) -> Option<&RefCell<super::ts_alias_resolution::StripInternalConfigs>> {
        let (dir, rev) = self.base.as_ref()?;
        self.base_configs
            .get_or_init(|| {
                super::ts_alias_resolution::BaseTree::load(dir, rev).map(|tree| {
                    RefCell::new(super::ts_alias_resolution::StripInternalConfigs::with_fs(
                        dir,
                        super::ts_alias_resolution::ConfigFs::Base(std::rc::Rc::new(tree)),
                    ))
                })
            })
            .as_ref()
    }

    /// 旧版を base の版の設定で判定するか。
    pub(crate) fn has_base(&self) -> bool {
        self.base_configs().is_some()
    }

    /// 旧版の判定に使う設定。
    fn old_configs(&self) -> Option<&RefCell<super::ts_alias_resolution::StripInternalConfigs>> {
        self.base_configs().or(self.configs.as_ref())
    }

    /// `path` (`dir` 相対。言語 `lang`、ソース `source`) で、`@internal` の宣言を宣言ファイルから
    /// 消すか (新版)。手書きの宣言ファイル (`.d.ts` など) は出力しないので消えない。
    pub(crate) fn applies(&self, path: &str, lang: LangId, source: &[u8]) -> bool {
        self.configs.as_ref().is_some_and(|configs| {
            may_strip(path, lang, source)
                && match self.overrides.borrow().get(&(false, path.to_owned())) {
                    Some(&value) => value,
                    None => configs.borrow_mut().strips(path),
                }
        })
    }

    /// [`Self::applies`] の旧版 (base の版の設定で判定する)。
    pub(crate) fn applies_old(&self, path: &str, lang: LangId, source: &[u8]) -> bool {
        // `@internal` を含まないソースでは base の版の設定を読まない (git を動かさない)。
        may_strip(path, lang, source)
            && self.old_configs().is_some_and(|configs| {
                match self.overrides.borrow().get(&(true, path.to_owned())) {
                    Some(&value) => value,
                    None => configs.borrow_mut().strips(path),
                }
            })
    }

    /// 旧版 `old_path` と新版 `new_path` の片方の設定を決められなければ、決められた側の判定を
    /// 両方に当てる (設定が変わったかを確かめられないので、変わらなかったとみなす)。旧新とも決め
    /// られなければ、両方とも有効にしないとみなす (読めない設定の扱いと同じ。片方だけが有効と
    /// 読めても、変化を結論しない)。決められない側があれば true (申告する)。
    pub(crate) fn align(&self, old_path: &str, new_path: &str) -> bool {
        use super::ts_alias_resolution::StripState;
        let Some((old, new)) = self.states(old_path, new_path) else {
            return false;
        };
        match (old, new) {
            (StripState::Unknown, StripState::Unknown) => {
                let mut overrides = self.overrides.borrow_mut();
                overrides.insert((true, old_path.to_owned()), false);
                overrides.insert((false, new_path.to_owned()), false);
                true
            }
            (StripState::Unknown, known) => {
                self.overrides
                    .borrow_mut()
                    .insert((true, old_path.to_owned()), known == StripState::Strips);
                true
            }
            (known, StripState::Unknown) => {
                self.overrides
                    .borrow_mut()
                    .insert((false, new_path.to_owned()), known == StripState::Strips);
                true
            }
            _ => false,
        }
    }

    /// `path` に `stripInternal` が当たるとき、宣言ファイルの型のメンバーにした `source`
    /// ([`published_source`])。当たらない・消すものが無い・解けないときは None (新版)。
    pub(crate) fn published(&self, path: &str, lang: LangId, source: &[u8]) -> Option<Vec<u8>> {
        if !self.applies(path, lang, source) {
            return None;
        }
        let tree = crate::engine::parser::parse_source(source, lang).ok()?;
        published_source(tree.root_node(), source)
    }

    /// [`Self::published`] の旧版。
    pub(crate) fn published_old(&self, path: &str, lang: LangId, source: &[u8]) -> Option<Vec<u8>> {
        if !self.applies_old(path, lang, source) {
            return None;
        }
        let tree = crate::engine::parser::parse_source(source, lang).ok()?;
        published_source(tree.root_node(), source)
    }

    /// 旧版 `old_path` と新版 `new_path` に当たる設定の判定 (`@internal` の有無を問わない)。
    /// base の版の設定を読めないときは None (旧新を比べ直さない)。
    pub(crate) fn states(
        &self,
        old_path: &str,
        new_path: &str,
    ) -> Option<(
        super::ts_alias_resolution::StripState,
        super::ts_alias_resolution::StripState,
    )> {
        let base = self.base_configs()?;
        let configs = self.configs.as_ref()?;
        let old = base.borrow_mut().strips_state(old_path);
        let new = configs.borrow_mut().strips_state(new_path);
        Some((old, new))
    }
}

/// `@internal` の宣言を宣言ファイルから消し得るソースか (TS の出力するソースで、`@internal` を
/// 含む)。手書きの宣言ファイル (`.d.ts` など) は出力しない。
pub(crate) fn may_strip(path: &str, lang: LangId, source: &[u8]) -> bool {
    matches!(lang, LangId::Typescript | LangId::Tsx)
        && !super::ts_module_paths::is_declaration_path(path)
        && source
            .windows(b"@internal".len())
            .any(|window| window == b"@internal")
}

/// TypeScript の改行 (`\n`・`\r`・U+2028・U+2029) があるか。
fn has_line_break(bytes: &[u8]) -> bool {
    bytes.iter().any(|byte| matches!(byte, b'\n' | b'\r'))
        || bytes
            .windows(3)
            .any(|window| matches!(window, [0xE2, 0x80, 0xA8 | 0xA9]))
}

/// 宣言 `node` の leading comment (ソースの順)。TypeScript の `getLeadingCommentRanges` と同じく、
/// 前のトークンの後の、コメントの外の最初の改行より後に始まるコメント (ファイルの先頭の宣言は
/// 先頭からの全て) で、前のトークンと同じ行のコメントと、その行から続く複数行のコメントは含まない。
fn leading_comments<'tree>(node: Node<'tree>, source: &[u8]) -> Vec<Node<'tree>> {
    let mut comments = Vec::new();
    let mut current = node.prev_sibling();
    let preceding = loop {
        match current {
            Some(sibling) if sibling.kind() == "comment" => {
                comments.push(sibling);
                current = sibling.prev_sibling();
            }
            other => break other,
        }
    };
    comments.reverse();
    let mut collecting = preceding.is_none();
    let mut cursor = preceding.map_or(0, |token| token.end_byte());
    let mut leading = Vec::new();
    for comment in comments {
        if !collecting {
            collecting = source
                .get(cursor..comment.start_byte())
                .is_none_or(has_line_break);
        }
        if collecting {
            leading.push(comment);
        }
        cursor = comment.end_byte();
    }
    leading
}

/// 宣言 `node` の leading comment に `@internal` があるか。TypeScript の `isInternalDeclaration`
/// と同じく、JSDoc に限らずコメントの字面で見る。
pub(crate) fn has_internal_comment(node: Node<'_>, source: &[u8]) -> bool {
    leading_comments(node, source).iter().any(|comment| {
        comment
            .utf8_text(source)
            .is_ok_and(|text| text.contains("@internal"))
    })
}

/// 宣言を並べる本体 (モジュール・namespace・クラス・インタフェース・型・enum)。
fn is_declaration_body(kind: &str) -> bool {
    matches!(
        kind,
        "program"
            | "statement_block"
            | "class_body"
            | "interface_body"
            | "object_type"
            | "enum_body"
    )
}

/// 型のメンバーの種類 (object 型と interface の本体の中)。
fn is_type_member(kind: &str) -> bool {
    matches!(
        kind,
        "property_signature"
            | "method_signature"
            | "call_signature"
            | "construct_signature"
            | "index_signature"
    )
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

fn named(node: Node<'_>, source: &[u8], name: &str) -> bool {
    node.child_by_field_name("name")
        .and_then(|name| name.utf8_text(source).ok())
        == Some(name)
}

/// `anchor` (本体 `body` の直下) が実装の関数・メソッドなら、同じ名前のオーバーロードの宣言
/// (宣言ファイルに出るのはこちら)。実装でなければ None。
fn overloads<'tree>(
    anchor: Node<'tree>,
    body: Node<'tree>,
    source: &[u8],
    name: &str,
) -> Option<Vec<Node<'tree>>> {
    let implementation = declaration_of(anchor);
    let signature_kind = match implementation.kind() {
        "function_declaration" => "function_signature",
        "method_definition" => "method_signature",
        _ => return None,
    };
    let mut cursor = body.walk();
    Some(
        body.named_children(&mut cursor)
            .filter(|statement| {
                let declaration = declaration_of(*statement);
                declaration.kind() == signature_kind && named(declaration, source, name)
            })
            .collect(),
    )
}

/// モジュールの `export { name }` 文 (from 句の無いもの)。
fn export_clause_statements<'tree>(
    program: Node<'tree>,
    source: &[u8],
    name: &str,
) -> Vec<Node<'tree>> {
    let mut cursor = program.walk();
    program
        .named_children(&mut cursor)
        .filter(|statement| {
            statement.kind() == "export_statement"
                && statement.child_by_field_name("source").is_none()
                && statement
                    .named_children(&mut statement.walk())
                    .filter(|clause| clause.kind() == "export_clause")
                    .any(|clause| {
                        clause
                            .named_children(&mut clause.walk())
                            .filter(|specifier| specifier.kind() == "export_specifier")
                            .any(|specifier| named(specifier, source, name))
                    })
        })
        .collect()
}

/// 本体の直下の宣言 `anchor` が、`@internal` で宣言ファイルから消えるか。実装の関数・メソッドは
/// オーバーロードの宣言が出るので、オーバーロードが全て `@internal` のとき消える。`export` の
/// 無い宣言を `export { name }` で公開しているなら、その文が全て `@internal` のときも公開から
/// 外れる (オーバーロードのある関数も)。
fn anchor_is_internal(anchor: Node<'_>, body: Node<'_>, source: &[u8], name: &str) -> bool {
    let declared_internal = match overloads(anchor, body, source, name) {
        Some(overloads) if !overloads.is_empty() => overloads
            .iter()
            .all(|overload| has_internal_comment(*overload, source)),
        _ => has_internal_comment(anchor, source),
    };
    if declared_internal {
        return true;
    }
    if body.kind() == "program" && anchor.kind() != "export_statement" {
        let statements = export_clause_statements(body, source, name);
        return !statements.is_empty()
            && statements
                .iter()
                .all(|statement| has_internal_comment(*statement, source));
    }
    false
}

/// シンボル `sym` の宣言が、それ自体か囲む宣言の `@internal` で宣言ファイルから消えるか。本体の
/// 直下まで上ったノード (export 文を含む) ごとに、その前のコメントを見る。式の中の本体 (クラス式の
/// メンバー) は推論した型として出るので、そのメンバーの `@internal` では消えない。
pub(crate) fn is_internal_symbol(sym: &Symbol, root: Node<'_>, source: &[u8]) -> bool {
    let start = tree_sitter::Point {
        row: sym.range.start.line,
        column: sym.range.start.column,
    };
    let end = tree_sitter::Point {
        row: sym.range.end.line,
        column: sym.range.end.column,
    };
    let Some(mut node) = root.descendant_for_point_range(start, end) else {
        return false;
    };
    let bare = sym.name.rsplit('.').next().unwrap_or(&sym.name);
    let mut innermost = true;
    while let Some(parent) = node.parent() {
        if is_declaration_body(parent.kind()) {
            let internal = is_emitted_position(parent)
                && if innermost {
                    anchor_is_internal(node, parent, source, bare)
                } else {
                    has_internal_comment(node, source)
                };
            if internal {
                return true;
            }
            innermost = false;
            if parent.kind() == "program" {
                return false;
            }
        }
        node = parent;
    }
    false
}

/// モジュールの `export { .. }` 文 (from 句の有無を問わない) が公開する名前のうち、`@internal` の
/// 文だけが公開するもの (宣言ファイルから消える)。
pub(crate) fn internal_only_export_names(
    root: Node<'_>,
    source: &[u8],
) -> std::collections::HashSet<String> {
    let mut internal = std::collections::HashSet::new();
    let mut public = std::collections::HashSet::new();
    let mut cursor = root.walk();
    for statement in root
        .named_children(&mut cursor)
        .filter(|statement| statement.kind() == "export_statement")
    {
        let target = if has_internal_comment(statement, source) {
            &mut internal
        } else {
            &mut public
        };
        let mut clauses = statement.walk();
        for clause in statement
            .named_children(&mut clauses)
            .filter(|clause| clause.kind() == "export_clause")
        {
            let mut specifiers = clause.walk();
            for specifier in clause
                .named_children(&mut specifiers)
                .filter(|specifier| specifier.kind() == "export_specifier")
            {
                let name = specifier
                    .child_by_field_name("alias")
                    .or_else(|| specifier.child_by_field_name("name"))
                    .and_then(|name| name.utf8_text(source).ok());
                if let Some(name) = name {
                    target.insert(name.to_owned());
                }
            }
        }
    }
    internal.retain(|name| !public.contains(name));
    internal
}

/// 宣言ファイルから消え得る型の要素か: object 型と interface のメンバー (mapped type の節は
/// 宣言ではないので消えない) と、タプルの要素 (ラベルの有無、`T?`・`...T[]` を問わない)。
fn is_strippable_type_element(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        "object_type" | "interface_body" => {
            is_type_member(node.kind())
                && !(node.kind() == "index_signature"
                    && node
                        .children(&mut node.walk())
                        .any(|child| child.kind() == "mapped_type_clause"))
        }
        "tuple_type" => node.is_named() && node.kind() != "comment",
        // クラスのメンバー (フィールド・メソッド・アクセサ・宣言・index signature) と enum の
        // メンバーも、宣言ファイルから消える (Issue #286)。
        "class_body" => matches!(
            node.kind(),
            "public_field_definition"
                | "method_definition"
                | "method_signature"
                | "abstract_method_signature"
                | "index_signature"
        ),
        "enum_body" => matches!(
            node.kind(),
            "property_identifier" | "string" | "number" | "enum_assignment"
        ),
        _ => false,
    }
}

/// 宣言ファイルを作るとき `stripInternal` で消える、本体の無い関数の宣言の文か (オーバーロードと
/// ambient の宣言。export 文も)。実装のある宣言とクラスなどは、シンボルごとに除く
/// ([`is_internal_symbol`])。オーバーロードの一部だけが `@internal` なら、残りのオーバーロードが
/// 宣言ファイルに出る (Issue #286)。
fn is_stripped_signature_statement(node: Node<'_>, source: &[u8]) -> bool {
    node.parent()
        .is_some_and(|parent| matches!(parent.kind(), "program" | "statement_block"))
        && declaration_of(node).kind() == "function_signature"
        && has_internal_comment(node, source)
        && is_emitted_position(node)
}

/// 宣言ファイルを作るとき `stripInternal` で消えるメンバーか ([`published_source`] が空白にする
/// もの)。
pub(crate) fn is_stripped_member(node: Node<'_>, source: &[u8]) -> bool {
    is_strippable_type_element(node)
        && has_internal_comment(node, source)
        && is_emitted_position(node)
}

/// 式の種類。式の中の型 (アロー関数の引数・`as` の型など) は、宣言ファイルでは推論した型として
/// 出るので、`@internal` のメンバーも消えない。
fn is_expression(kind: &str) -> bool {
    matches!(
        kind,
        "arrow_function"
            | "function_expression"
            | "generator_function"
            | "class"
            | "as_expression"
            | "satisfies_expression"
            | "type_assertion"
            | "call_expression"
            | "new_expression"
            | "object"
            | "array"
            | "pair"
            | "spread_element"
            | "arguments"
            | "parenthesized_expression"
            | "sequence_expression"
            | "assignment_expression"
            | "augmented_assignment_expression"
            | "binary_expression"
            | "unary_expression"
            | "update_expression"
            | "ternary_expression"
            | "await_expression"
            | "yield_expression"
            | "instantiation_expression"
            | "member_expression"
            | "subscript_expression"
            | "non_null_expression"
            | "template_string"
            | "decorator"
    )
}

/// `node` (`export_statement` の直下) が、名前の無い `export default` のクラス・関数の宣言か。
/// 文法では式のノードになるが、TypeScript では宣言として宣言ファイルにそのまま写す。
fn is_default_export_declaration(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "class" | "function_expression" | "generator_function"
    ) && node.parent().is_some_and(|parent| {
        parent.kind() == "export_statement"
            && parent
                .children(&mut parent.walk())
                .any(|child| child.kind() == "default")
    })
}

/// `node` が、宣言ファイルを作るとき TypeScript が宣言や型をそのまま写す位置にあるか。式の中
/// (推論した型として出る) と関数の本体の中 (出ない) は写さない。型の中の `typeof` の式
/// (`typeof f<{ .. }>` の型引数など) は型として写す。
fn is_emitted_position(node: Node<'_>) -> bool {
    let mut child = node;
    // 下に式があったか (上に `typeof` の型があれば、その式は型の一部)。
    let mut in_expression = false;
    while let Some(parent) = child.parent() {
        if parent.kind() == "type_query" {
            in_expression = false;
        } else if is_expression(parent.kind()) && !is_default_export_declaration(parent) {
            in_expression = true;
        }
        if matches!(
            parent.kind(),
            "function_declaration"
                | "generator_function_declaration"
                | "method_definition"
                | "class_static_block"
        ) && parent
            .child_by_field_name("body")
            .is_some_and(|body| body.id() == child.id())
        {
            return false;
        }
        child = parent;
    }
    !in_expression
}

/// 宣言ファイルの型のメンバー: `source` の、宣言ファイルから消える `@internal` の型の要素
/// (interface と object 型のメンバーとラベル付きのタプルの要素。関数の引数や戻り値・交差型の中
/// など、入れ子の型も) を空白にしたもの。宣言ファイルに型をそのまま写す位置のものだけで、式の中の
/// 型 (推論した型として出る) は残す。要素と一緒に消える leading comment・区切り・trailing comment
/// も空白にする。改行は残すので、位置 (バイト・行・列) は元のソースと変わらない。消すものが無ければ
/// None。
pub(crate) fn published_source(root: Node<'_>, source: &[u8]) -> Option<Vec<u8>> {
    let mut ranges = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if is_stripped_member(node, source) || is_stripped_signature_statement(node, source) {
            ranges.push(member_extent(node, source));
            continue;
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    if ranges.is_empty() {
        return None;
    }
    let mut published = source.to_vec();
    for range in ranges {
        for byte in published.get_mut(range)? {
            if !matches!(*byte, b'\n' | b'\r') {
                *byte = b' ';
            }
        }
    }
    Some(published)
}

/// 宣言ファイルから消える型の要素 `member` の範囲。TypeScript ではメンバーの区切り (`;`・`,`)
/// はメンバーの一部なので、間のコメントを越えて区切りまでを含める。leading comment と、続く
/// 同じ行のコメント (trailing comment) もメンバーと一緒に消える。
fn member_extent(member: Node<'_>, source: &[u8]) -> Range<usize> {
    let start = leading_comments(member, source)
        .first()
        .map_or(member.start_byte(), Node::start_byte);
    let mut end = member.end_byte();
    let mut next = member.next_sibling();
    while let Some(comment) = next.filter(|node| node.kind() == "comment") {
        next = comment.next_sibling();
    }
    match next.filter(|node| matches!(node.kind(), ";" | ",")) {
        Some(separator) => {
            end = separator.end_byte();
            next = separator.next_sibling();
        }
        None => next = member.next_sibling(),
    }
    while let Some(comment) = next.filter(|node| node.kind() == "comment") {
        if source
            .get(end..comment.start_byte())
            .is_none_or(has_line_break)
        {
            break;
        }
        end = comment.end_byte();
        next = comment.next_sibling();
    }
    start..end
}
