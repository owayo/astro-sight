//! Python の関数・メソッドで戻り値の注釈だけを変えた変更の、呼び出し側の追随の証拠 (Issue #251)。
//!
//! 戻り値の契約だけが変わると、呼び出し側が直すのは結果を使う文 (`assert result == 0` など) で、
//! 呼び出しの文 (`result = entry()`) は変わらないことが多い。git diff はその文を旧版の同じ文字列の
//! 行と対応付けて context 行にするので、呼び出しの行だけを見ると、新しく書いた呼び出しでも
//! 追随していないように見える。
//!
//! ここでは、結果を受けた変数の読み取りを全部たどり、どれも新しい戻り値の型に合う値との比較
//! (assert の条件か if / elif / while の条件) か assert の message の中にあり、その文が旧版のどの
//! 文とも一致しない (書き換えた) ことを、呼び出しの追随の証拠にする。比較の相手は、注釈の型の
//! リテラルに限る (変数・呼び出しの結果と比べても、新しい契約に合わせたとは言えない)。読み取りを
//! 漏れなく追えない形 (別名や関数への受け渡し・再代入・入れ子のスコープ・`locals()` など) や、
//! 旧版を読めない・解析できないときは証拠にせず、呼び出しの行で見る従来の判定のままにする。

use std::collections::HashSet;
use std::fmt::Write as _;
use std::ops::ControlFlow;
use std::rc::Rc;

use tree_sitter::Node;

use super::ref_index::{ApiClosureCaches, parsed_ref_file_shared};
use super::{
    CompatibleModSite, SignatureSourceCache, find_python_function_by_name,
    python_function_signature_parts,
};
use crate::commands::git_input::GitBlobBatch;
use crate::engine::tree_walk::{Children, Walk, walk_preorder};
use crate::language::LangId;
use crate::models::impact::DiffFile;
use crate::models::reference::SymbolReference;

/// 入れ子のスコープを作るノード。この中の同名の識別子は、呼び出しの結果を受けた束縛とは
/// 限らない (同名の引数・内包表記の変数・クロージャへの捕捉)。
const NESTED_SCOPES: &[&str] = &[
    "function_definition",
    "class_definition",
    "lambda",
    "list_comprehension",
    "set_comprehension",
    "dictionary_comprehension",
    "generator_expression",
];

/// tree-sitter-python の単純文 (`_simple_statement`)。旧版の文の単位にする。
const SIMPLE_STATEMENTS: &[&str] = &[
    "assert_statement",
    "break_statement",
    "continue_statement",
    "delete_statement",
    "exec_statement",
    "expression_statement",
    "future_import_statement",
    "global_statement",
    "import_from_statement",
    "import_statement",
    "nonlocal_statement",
    "pass_statement",
    "print_statement",
    "raise_statement",
    "return_statement",
    "type_alias_statement",
];

/// 条件式を文の単位にする複合文と節。
const CONDITIONAL: &[&str] = &["if_statement", "elif_clause", "while_statement"];

/// 名前空間を文字列で読み書きする組み込み関数。関数の中にあれば、識別子をたどっても
/// 変数の読み取りを漏れなく数えたことにならない。
const DYNAMIC_SCOPE: &[&str] = &["locals", "vars", "globals", "eval", "exec"];

/// 変更後の関数が返す値の型 (戻り値の注釈)。単純な組み込み型の名前と `None` だけを扱う。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReturnType {
    Int,
    Float,
    Str,
    Bytes,
    Bool,
    NoneType,
}

/// 戻り値の注釈だけを変えた Python の関数・メソッドの、新しい戻り値の契約。
pub(crate) struct PythonReturnContract<'a> {
    /// base の旧版を読む (呼び出し側のファイルの旧版と比べる)。
    base_blobs: &'a GitBlobBatch,
    returns: ReturnType,
    /// `async def`。結果は `await` して受けたときだけ戻り値になる。
    is_async: bool,
}

/// 変更が戻り値の注釈だけで、新しい注釈が扱える型なら、その契約を返す。
///
/// 旧新の定義を AST で比べ、`async`・名前・型引数・引数・すべてのデコレータが同じことを求める
/// (引数が変われば呼び出し式の追随が要り、デコレータは戻り値や引数の扱いを変え得る)。新しい
/// 注釈の名前をモジュールのどこかで束縛し得るとき (組み込み型と限らない) と、generator
/// (呼び出しの結果が注釈の型の値でない) は扱わない。
pub(crate) fn prepare<'a>(
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
    base_blobs: &'a GitBlobBatch,
) -> Option<PythonReturnContract<'a>> {
    let lang = site.lang_in(&[LangId::Python])?;
    if site.kind != "function" && site.kind != "method" {
        return None;
    }
    let pair = sources.get(site)?;
    let (old_tree, new_tree) = pair.parse_pair(lang)?;
    let (old_root, new_root) = (old_tree.root_node(), new_tree.root_node());
    if old_root.has_error() || new_root.has_error() {
        return None;
    }
    let old = find_python_function_by_name(old_root, &pair.old, site.name)?;
    let new = find_python_function_by_name(new_root, &pair.new, site.name)?;
    if !python_function_signature_parts(old, &pair.old)?
        .differs_only_in_return_annotation(&python_function_signature_parts(new, &pair.new)?)
    {
        return None;
    }
    let returns = ReturnType::of(new, &pair.new)?;
    if returns
        .builtin_name()
        .is_some_and(|name| may_rebind(new_root, &pair.new, name))
        || is_generator(new)
    {
        return None;
    }
    Some(PythonReturnContract {
        base_blobs,
        returns,
        is_async: new.child(0).is_some_and(|token| token.kind() == "async"),
    })
}

impl PythonReturnContract<'_> {
    /// `reference` (呼び出しの callee) の結果の読み取りが、すべて新しい契約に合わせて書き換えた
    /// 文にあるか。証明できなければ false (呼び出し側は従来の判定のまま)。
    pub(super) fn closes_reference(
        &self,
        dir: &str,
        diff_file: &DiffFile,
        reference: &SymbolReference,
        callee: &str,
        caches: &mut ApiClosureCaches,
    ) -> bool {
        // 新規ファイルは全行が追加行なので、呼び出しの行で見る判定が先に閉じる。
        if diff_file.old_path == "/dev/null" {
            return false;
        }
        let Some(file) = parsed_ref_file_shared(dir, &reference.path, caches) else {
            return false;
        };
        if file.lang_id != LangId::Python || file.tree.root_node().has_error() {
            return false;
        }
        let Some(units) = self.rewritten_read_units(
            file.tree.root_node(),
            &file.source,
            callee,
            reference.line,
            reference.column,
        ) else {
            return false;
        };
        let Some(old_units) = old_statement_units(self.base_blobs, &diff_file.old_path, caches)
        else {
            return false;
        };
        units.iter().all(|unit| !old_units.contains(unit))
    }

    /// 呼び出しの結果を受けた変数の読み取りを含む文の単位 (トークン列のキー)。
    fn rewritten_read_units(
        &self,
        root: Node<'_>,
        source: &[u8],
        callee: &str,
        line: usize,
        column: usize,
    ) -> Option<Vec<String>> {
        self.read_unit_nodes(root, source, callee, line, column)?
            .into_iter()
            .map(|unit| token_key(unit, source))
            .collect()
    }

    /// 呼び出しの結果を受けた変数の読み取りを含む文の単位。読み取りがすべて新しい戻り値の型に
    /// 合う比較か assert の message の中にあり、比較が 1 つ以上あるときだけ返す。
    fn read_unit_nodes<'tree>(
        &self,
        root: Node<'tree>,
        source: &[u8],
        callee: &str,
        line: usize,
        column: usize,
    ) -> Option<Vec<Node<'tree>>> {
        let binding = ResultBinding::find(root, source, callee, line, column, self.is_async)?;
        let mut units: Vec<Node<'tree>> = Vec::new();
        let mut compared = false;
        let walked = walk_with_path(binding.body, |path| {
            let &(node, _) = path.last().expect("walk_with_path passes the current node");
            // 属性名 (`obj.result`) とキーワード引数名 (`f(result=1)`) は、同じ綴りでも名前の参照ではない。
            if node.kind() != "identifier" || !names_a_binding(path) {
                return ControlFlow::Continue(Walk::Descend);
            }
            let Ok(text) = node.utf8_text(source) else {
                return ControlFlow::Break(());
            };
            if DYNAMIC_SCOPE.contains(&text) {
                return ControlFlow::Break(());
            }
            if text != binding.variable || node.id() == binding.target.id() {
                return ControlFlow::Continue(Walk::Descend);
            }
            match self.classify_read(path, &binding, source) {
                None => ControlFlow::Break(()),
                Some(read) => {
                    compared |= read.compared;
                    if !units.iter().any(|unit| unit.id() == read.unit.id()) {
                        units.push(read.unit);
                    }
                    ControlFlow::Continue(Walk::Descend)
                }
            }
        });
        (walked.is_continue() && compared).then_some(units)
    }

    /// 変数の出現 1 つを分類する。None は追えない出現 (証拠にしない)。
    fn classify_read<'tree>(
        &self,
        path: &Path<'tree>,
        binding: &ResultBinding<'tree>,
        source: &[u8],
    ) -> Option<Read<'tree>> {
        let (&(node, _), ancestors) = path.split_last()?;
        let &(parent, _) = ancestors.last()?;
        // 入れ子のスコープの中の出現と、代入より前 (代入の文の中を含む) の出現は、呼び出しの
        // 結果を受けた束縛を読むとは限らない。
        if ancestors
            .iter()
            .skip(1)
            .any(|(ancestor, _)| NESTED_SCOPES.contains(&ancestor.kind()))
            || node.start_byte() < binding.statement.end_byte()
        {
            return None;
        }
        if parent.kind() == "comparison_operator"
            && let Some(unit) = self.compared_unit(path, source)
        {
            return Some(Read {
                unit,
                compared: true,
            });
        }
        assert_message_unit(path).map(|unit| Read {
            unit,
            compared: false,
        })
    }

    /// 変数を新しい戻り値の型のリテラルと比べる比較が、assert の条件か if / elif / while の条件
    /// そのもの (括弧は外す) なら、その文の単位 (assert の文、または条件式) を返す。
    fn compared_unit<'tree>(&self, path: &Path<'tree>, source: &[u8]) -> Option<Node<'tree>> {
        let variable_index = path.len().checked_sub(1)?;
        let comparison_index = variable_index.checked_sub(1)?;
        let (variable, comparison) = (path[variable_index].0, path[comparison_index].0);
        let operands = named_children(comparison);
        let [left, right] = operands.as_slice() else {
            return None;
        };
        let mut cursor = comparison.walk();
        let operators: Vec<Node<'_>> = comparison
            .children_by_field_name("operators", &mut cursor)
            .collect();
        let [operator] = operators.as_slice() else {
            return None;
        };
        let (other, variable_on_left) = if left.id() == variable.id() {
            (*right, true)
        } else if right.id() == variable.id() {
            (*left, false)
        } else {
            return None;
        };
        if !self
            .returns
            .accepts_comparison(operator.kind(), other, variable_on_left, source)
        {
            return None;
        }
        let mut index = comparison_index;
        while index > 0 && path[index - 1].0.kind() == "parenthesized_expression" {
            index -= 1;
        }
        let (condition, condition_field) = path[index];
        let (holder, _) = path[index.checked_sub(1)?];
        match holder.kind() {
            "assert_statement" => {
                (named_children(holder).first()?.id() == condition.id()).then_some(holder)
            }
            kind if CONDITIONAL.contains(&kind) => {
                (condition_field == Some("condition")).then_some(condition)
            }
            _ => None,
        }
    }
}

/// 根から現在のノードまでの経路 (ノードと、親の中でのフィールド名)。
type Path<'tree> = [(Node<'tree>, Option<&'tree str>)];

/// 変数の読み取り 1 つ。
struct Read<'tree> {
    /// 読み取りを含む文の単位。旧版の文と比べる。
    unit: Node<'tree>,
    /// 新しい戻り値の型のリテラルとの比較か (assert の message の中なら false)。
    compared: bool,
}

/// 呼び出しの結果を受けた `v = <call>` の代入。
struct ResultBinding<'tree> {
    /// 代入を囲む関数の本体。
    body: Node<'tree>,
    /// 代入の文。
    statement: Node<'tree>,
    /// 代入先の識別子。
    target: Node<'tree>,
    variable: String,
}

impl<'tree> ResultBinding<'tree> {
    /// `(line, column)` の callee の呼び出し (`entry()` / `obj.entry()`) が、型注釈の無い単純代入
    /// `v = <call>` (async 関数なら `v = await <call>`) の右辺そのもので、関数本体の中にあれば
    /// その代入を返す。モジュール直下・クラス直下の代入は、読み取りが関数をまたぐので扱わない。
    fn find(
        root: Node<'tree>,
        source: &[u8],
        callee: &str,
        line: usize,
        column: usize,
        awaited: bool,
    ) -> Option<Self> {
        let point = tree_sitter::Point { row: line, column };
        let name = root
            .descendant_for_point_range(point, point)
            .filter(|node| {
                node.kind() == "identifier" && node.utf8_text(source).ok() == Some(callee)
            })?;
        let parent = name.parent()?;
        let function = match parent.kind() {
            "call" => name,
            "attribute"
                if parent
                    .child_by_field_name("attribute")
                    .is_some_and(|attribute| attribute.id() == name.id()) =>
            {
                parent
            }
            _ => return None,
        };
        let call = function.parent().filter(|node| node.kind() == "call")?;
        if call
            .child_by_field_name("function")
            .is_none_or(|node| node.id() != function.id())
        {
            return None;
        }
        let value = match call.parent()? {
            node if node.kind() == "await" => node,
            _ => call,
        };
        // async 関数の戻り値は await して受けたときだけ得られる。await しない結果は coroutine。
        if (value.kind() == "await") != awaited {
            return None;
        }
        let assignment = value.parent().filter(|node| node.kind() == "assignment")?;
        if assignment
            .child_by_field_name("right")
            .is_none_or(|node| node.id() != value.id())
            || assignment.child_by_field_name("type").is_some()
        {
            return None;
        }
        let target = assignment
            .child_by_field_name("left")
            .filter(|node| node.kind() == "identifier")?;
        let statement = assignment.parent().filter(|node| {
            node.kind() == "expression_statement" && named_children(*node).len() == 1
        })?;
        statement.parent().filter(|node| node.kind() == "block")?;
        Some(Self {
            body: enclosing_function_body(root, statement)?,
            statement,
            target,
            variable: target.utf8_text(source).ok()?.to_owned(),
        })
    }
}

/// `statement` を囲む最も内側のスコープが関数で、`statement` がその本体の中にあれば本体を返す。
/// 祖先は根から降りて求める (`Node::parent` を繰り返すと深さの 2 乗かかる)。
fn enclosing_function_body<'tree>(
    root: Node<'tree>,
    statement: Node<'tree>,
) -> Option<Node<'tree>> {
    let mut cursor = root.walk();
    let mut scope = None;
    while cursor.node().id() != statement.id() {
        let node = cursor.node();
        if NESTED_SCOPES.contains(&node.kind()) {
            scope = Some(node);
        }
        cursor.goto_first_child_for_byte(statement.start_byte())?;
    }
    let body = scope
        .filter(|node: &Node<'tree>| node.kind() == "function_definition")?
        .child_by_field_name("body")?;
    (body.start_byte() <= statement.start_byte() && statement.end_byte() <= body.end_byte())
        .then_some(body)
}

/// 経路の末尾の識別子が名前の参照か (属性名 `obj.name` とキーワード引数名 `f(name=…)` は違う)。
fn names_a_binding(path: &Path<'_>) -> bool {
    let Some((&(_, field), ancestors)) = path.split_last() else {
        return false;
    };
    let Some(&(parent, _)) = ancestors.last() else {
        return true;
    };
    !matches!(
        (parent.kind(), field),
        ("attribute", Some("attribute")) | ("keyword_argument", Some("name"))
    )
}

/// `assert <条件>, <message>` の message の中の読み取りなら、その assert 文を返す。message は
/// 失敗したときにだけ評価され、条件の真偽を変えない。message の中で別の名前を束縛する式
/// (walrus) があれば扱わない。
fn assert_message_unit<'tree>(path: &Path<'tree>) -> Option<Node<'tree>> {
    let index = path
        .iter()
        .rposition(|(node, _)| node.kind() == "assert_statement")?;
    let statement = path[index].0;
    let part = path.get(index + 1)?.0;
    if named_children(statement).first()?.id() == part.id()
        || path[index + 1..]
            .iter()
            .any(|(node, _)| node.kind() == "named_expression")
    {
        return None;
    }
    Some(statement)
}

impl ReturnType {
    /// 新しい関数の戻り値の注釈。
    fn of(function: Node<'_>, source: &[u8]) -> Option<Self> {
        let annotation = function.child_by_field_name("return_type")?;
        let [value] = named_children(annotation).try_into().ok()?;
        match value.kind() {
            "none" => Some(Self::NoneType),
            "identifier" => match value.utf8_text(source).ok()? {
                "int" => Some(Self::Int),
                "float" => Some(Self::Float),
                "str" => Some(Self::Str),
                "bytes" => Some(Self::Bytes),
                "bool" => Some(Self::Bool),
                _ => None,
            },
            _ => None,
        }
    }

    /// 注釈に書いた名前。`None` は予約語なので束縛されない。
    fn builtin_name(self) -> Option<&'static str> {
        match self {
            Self::Int => Some("int"),
            Self::Float => Some("float"),
            Self::Str => Some("str"),
            Self::Bytes => Some("bytes"),
            Self::Bool => Some("bool"),
            Self::NoneType => None,
        }
    }

    /// 結果を `operator` で `other` と比べる式 (`variable_on_left` が false なら左右が逆) が、
    /// この型の値の比較として書けているか。
    fn accepts_comparison(
        self,
        operator: &str,
        other: Node<'_>,
        variable_on_left: bool,
        source: &[u8],
    ) -> bool {
        match operator {
            "==" | "!=" => self.literal(other, source),
            "<" | "<=" | ">" | ">=" => {
                matches!(self, Self::Int | Self::Float | Self::Str | Self::Bytes)
                    && self.literal(other, source)
            }
            "is" | "is not" => {
                matches!(self, Self::NoneType | Self::Bool) && self.literal(other, source)
            }
            "in" | "not in" => {
                let collection = unparenthesized(other);
                let elements = named_children(collection);
                variable_on_left
                    && matches!(collection.kind(), "tuple" | "list" | "set")
                    && !elements.is_empty()
                    && elements
                        .iter()
                        .all(|element| self.literal(*element, source))
            }
            _ => false,
        }
    }

    /// `node` がこの型のリテラルか。
    fn literal(self, node: Node<'_>, source: &[u8]) -> bool {
        let node = unparenthesized(node);
        match self {
            Self::Int => signed_literal(node, "integer"),
            Self::Float => signed_literal(node, "integer") || signed_literal(node, "float"),
            Self::Str => string_literal_is_bytes(node, source) == Some(false),
            Self::Bytes => string_literal_is_bytes(node, source) == Some(true),
            Self::Bool => matches!(node.kind(), "true" | "false"),
            Self::NoneType => node.kind() == "none",
        }
    }
}

/// `kind` のリテラル、または単項の `+` / `-` を付けたもの。
fn signed_literal(node: Node<'_>, kind: &str) -> bool {
    node.kind() == kind
        || (node.kind() == "unary_operator"
            && node
                .child_by_field_name("operator")
                .is_some_and(|operator| matches!(operator.kind(), "+" | "-"))
            && node
                .child_by_field_name("argument")
                .is_some_and(|argument| argument.kind() == kind))
}

/// 文字列リテラル (隣り合う文字列の連結を含む) なら bytes かどうかを返す。文字列でないとき・
/// bytes と str が混ざるときは None。
fn string_literal_is_bytes(node: Node<'_>, source: &[u8]) -> Option<bool> {
    let parts = match node.kind() {
        "string" => vec![node],
        "concatenated_string" => named_children(node),
        _ => return None,
    };
    let mut kinds = parts.iter().map(|part| {
        let start = (part.kind() == "string")
            .then(|| part.child(0))
            .flatten()
            .filter(|start| start.kind() == "string_start")?;
        let prefix = start.utf8_text(source).ok()?.trim_end_matches(['"', '\'']);
        Some(prefix.contains(['b', 'B']))
    });
    let first = kinds.next()??;
    kinds.all(|kind| kind == Some(first)).then_some(first)
}

/// 括弧で 1 つの式だけを囲んだものを外す。
fn unparenthesized(mut node: Node<'_>) -> Node<'_> {
    while node.kind() == "parenthesized_expression" {
        match named_children(node).as_slice() {
            [inner] => node = *inner,
            _ => break,
        }
    }
    node
}

/// コメントと行継続を除いた名前付きの子。
fn named_children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| !child.is_extra())
        .collect()
}

/// 関数の本体に `yield` があるか (入れ子の関数・lambda・クラスは除く)。generator の呼び出しは
/// 注釈の型の値を返さない。
fn is_generator(function: Node<'_>) -> bool {
    let Some(body) = function.child_by_field_name("body") else {
        return true;
    };
    walk_preorder(body, Children::Named, |node, _| match node.kind() {
        "yield" => ControlFlow::Break(()),
        "function_definition" | "lambda" | "class_definition" => ControlFlow::Continue(Walk::Skip),
        _ => ControlFlow::Continue(Walk::Descend),
    })
    .is_break()
}

/// モジュールのどこかで `name` を束縛し得るか。束縛の位置 (代入・for・with / except の as・walrus・
/// 定義の名前・引数・global / nonlocal / del・match のパターン・import) に現れるか、star import が
/// あれば true。関数の中の束縛も数える (保守側に広く数える)。
fn may_rebind(root: Node<'_>, source: &[u8], name: &str) -> bool {
    walk_with_path(root, |path| {
        let &(node, field) = path.last().expect("walk_with_path passes the current node");
        if node.kind() == "wildcard_import" {
            return ControlFlow::Break(());
        }
        if node.kind() != "identifier" || node.utf8_text(source).ok() != Some(name) {
            return ControlFlow::Continue(Walk::Descend);
        }
        let in_declaration = path.iter().any(|(ancestor, _)| {
            matches!(
                ancestor.kind(),
                "import_statement"
                    | "import_from_statement"
                    | "future_import_statement"
                    | "type_alias_statement"
            )
        });
        let parent = path.len().checked_sub(2).map(|index| path[index].0.kind());
        if in_declaration || parent.is_some_and(|parent| binding_position(parent, field)) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(Walk::Descend)
        }
    })
    .is_break()
}

/// 親が `parent` で、親の中のフィールドが `field` の識別子が、名前を束縛し得る位置か。
fn binding_position(parent: &str, field: Option<&str>) -> bool {
    match parent {
        "assignment" | "augmented_assignment" | "for_statement" | "for_in_clause" => {
            field == Some("left")
        }
        "named_expression"
        | "function_definition"
        | "class_definition"
        | "default_parameter"
        | "typed_default_parameter" => field == Some("name"),
        "typed_parameter" => field != Some("type"),
        "parameters"
        | "lambda_parameters"
        | "pattern_list"
        | "tuple_pattern"
        | "list_pattern"
        | "list_splat_pattern"
        | "dictionary_splat_pattern"
        | "as_pattern_target"
        | "global_statement"
        | "nonlocal_statement"
        | "delete_statement"
        | "dotted_name"
        | "case_pattern"
        | "splat_pattern"
        | "keyword_pattern" => true,
        _ => false,
    }
}

/// 旧版の文の単位 (単純文の全体と、if / elif / while の条件式) のトークン列のキー。旧版のパス
/// ごとに 1 回だけ作る。読めない・上限を超える・解析エラーがあるときは None。
fn old_statement_units(
    base_blobs: &GitBlobBatch,
    old_path: &str,
    caches: &mut ApiClosureCaches,
) -> Option<Rc<HashSet<String>>> {
    caches
        .python_old_statement_units
        .entry(old_path.to_owned())
        .or_insert_with(|| {
            let limit = usize::try_from(crate::engine::parser::MAX_FILE_SIZE).ok()?;
            let source = base_blobs.read_limited(old_path, limit)?;
            let tree = crate::engine::parser::parse_source(&source, LangId::Python).ok()?;
            let root = tree.root_node();
            if root.has_error() {
                return None;
            }
            let mut units = HashSet::new();
            let walked = walk_preorder(root, Children::Named, |node, _| {
                let unit = if CONDITIONAL.contains(&node.kind()) {
                    node.child_by_field_name("condition")
                } else {
                    SIMPLE_STATEMENTS.contains(&node.kind()).then_some(node)
                };
                if let Some(unit) = unit {
                    let Some(key) = token_key(unit, &source) else {
                        return ControlFlow::Break(());
                    };
                    units.insert(key);
                }
                ControlFlow::Continue(Walk::Descend)
            });
            walked.is_continue().then(|| Rc::new(units))
        })
        .clone()
}

/// 文の単位のトークン列 (コメントと行継続を除く。文字列は中の空白を含めて 1 トークン) のキー。
/// 種別・長さ・原文を並べ、トークンの境界を曖昧にしない。UTF-8 でないトークンがあれば None。
fn token_key(node: Node<'_>, source: &[u8]) -> Option<String> {
    let mut key = String::new();
    let walked = walk_preorder(node, Children::All, |token, _| {
        if token.is_extra() {
            return ControlFlow::Continue(Walk::Skip);
        }
        if token.kind() != "string" && token.child_count() > 0 {
            return ControlFlow::Continue(Walk::Descend);
        }
        let Ok(text) = token.utf8_text(source) else {
            return ControlFlow::Break(());
        };
        let _ = writeln!(key, "{}:{}:{text}", token.kind(), text.len());
        ControlFlow::Continue(Walk::Skip)
    });
    walked.is_continue().then_some(key)
}

/// `root` の部分木を前順に訪れ、各ノードで `root` からの経路 (末尾が現在のノード) を渡す。
/// `Walk::Skip` を返すと子孫を飛ばし、`ControlFlow::Break` で走査を終える。再帰しない。
fn walk_with_path<'tree>(
    root: Node<'tree>,
    mut visit: impl FnMut(&Path<'tree>) -> ControlFlow<(), Walk>,
) -> ControlFlow<()> {
    let mut cursor = root.walk();
    let mut path: Vec<(Node<'tree>, Option<&'tree str>)> = Vec::new();
    loop {
        path.push((cursor.node(), cursor.field_name()));
        if visit(&path)? == Walk::Descend && cursor.goto_first_child() {
            continue;
        }
        loop {
            path.pop();
            if path.is_empty() {
                return ControlFlow::Continue(());
            }
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return ControlFlow::Continue(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::parser;

    fn contract(
        blobs: &GitBlobBatch,
        returns: ReturnType,
        is_async: bool,
    ) -> PythonReturnContract<'_> {
        PythonReturnContract {
            base_blobs: blobs,
            returns,
            is_async,
        }
    }

    /// `marked` の `@` の位置の `entry` の呼び出しについて、読み取りの文の単位の原文 (空白を
    /// 詰めたもの) を返す。
    fn read_units(contract: &PythonReturnContract<'_>, marked: &str) -> Option<Vec<String>> {
        let offset = marked.find('@').expect("marker");
        let source = marked.replacen('@', "", 1);
        let prefix = &source[..offset];
        let line = prefix.bytes().filter(|byte| *byte == b'\n').count();
        let column = offset - prefix.rfind('\n').map_or(0, |index| index + 1);
        let tree = parser::parse_source(source.as_bytes(), LangId::Python).unwrap();
        assert!(!tree.root_node().has_error(), "{marked}");
        let units =
            contract.read_unit_nodes(tree.root_node(), source.as_bytes(), "entry", line, column)?;
        Some(
            units
                .iter()
                .map(|unit| {
                    crate::engine::normalize_whitespace(&source.as_bytes()[unit.byte_range()])
                })
                .collect(),
        )
    }

    /// 結果の読み取りは、新しい戻り値の型のリテラルとの比較 (assert / if / elif / while の条件)
    /// と assert の message だけを数える。追えない出現が 1 つでもあれば証拠にしない。
    #[test]
    fn result_reads_must_compare_with_a_literal_of_the_new_return_type() {
        let blobs = GitBlobBatch::new(".", "HEAD");
        let int = contract(&blobs, ReturnType::Int, false);
        for (marked, expected) in [
            (
                "def t():\n    result = @entry()\n    assert result == 0\n",
                Some(vec!["assert result == 0"]),
            ),
            (
                "def t():\n    result = @entry()\n    assert 0 != result, f\"got {result}\"\n",
                Some(vec!["assert 0 != result, f\"got {result}\""]),
            ),
            (
                "def t():\n    result = @entry()\n    assert result in (0, 1)\n    if (result >= -1):\n        pass\n",
                Some(vec!["assert result in (0, 1)", "(result >= -1)"]),
            ),
            (
                "def t():\n    result = mod.@entry()\n    while result != 0:\n        break\n",
                Some(vec!["result != 0"]),
            ),
            // 属性名とキーワード引数名は、同じ名前でも変数の読み取りではない。
            (
                "def t():\n    result = @entry()\n    other.result(result=1)\n    assert result == 0, other.result\n",
                Some(vec!["assert result == 0, other.result"]),
            ),
            (
                "def t():\n    result = @entry()\n    print(request.vars, f(locals=1))\n    assert result == 0\n",
                Some(vec!["assert result == 0"]),
            ),
            // 比較の相手が型の合わないリテラル・変数、比較の形・位置が違う
            (
                "def t():\n    result = @entry()\n    assert result == \"0\"\n",
                None,
            ),
            (
                "def t():\n    result = @entry()\n    assert result == expected\n",
                None,
            ),
            (
                "def t():\n    result = @entry()\n    assert 0 in result\n",
                None,
            ),
            (
                "def t():\n    result = @entry()\n    assert result is 0\n",
                None,
            ),
            (
                "def t():\n    result = @entry()\n    assert 0 < result < 2\n",
                None,
            ),
            (
                "def t():\n    result = @entry()\n    assert result.real == 0\n",
                None,
            ),
            (
                "def t():\n    result = @entry()\n    assert result == 0 or True\n",
                None,
            ),
            (
                "def t():\n    result = @entry()\n    return result == 0\n",
                None,
            ),
            // 結果が別の名前・関数へ渡る
            (
                "def t():\n    result = @entry()\n    check(result)\n    assert result == 0\n",
                None,
            ),
            (
                "def t():\n    result = @entry()\n    value = result\n    assert value == 0\n",
                None,
            ),
            (
                "def t():\n    result = @entry()\n    assert result == 0, (n := result)\n",
                None,
            ),
            (
                "def t():\n    result = @entry()\n    assert (lambda: result == 0)()\n",
                None,
            ),
            (
                "def t():\n    result = @entry()\n    assert [r for r in [result]] == [0]\n",
                None,
            ),
            // 再代入・代入より前の読み取り・動的な名前空間・削除・読み取り無し
            (
                "def t():\n    result = @entry()\n    result = 1\n    assert result == 0\n",
                None,
            ),
            (
                "def t():\n    print(result)\n    result = @entry()\n    assert result == 0\n",
                None,
            ),
            (
                "def t():\n    result = @entry()\n    assert result == 0\n    print(locals())\n",
                None,
            ),
            ("def t():\n    result = @entry()\n    del result\n", None),
            ("def t():\n    result = @entry()\n", None),
            // 呼び出しの受け方が単純代入でない・関数の外
            (
                "def t():\n    result: int = @entry()\n    assert result == 0\n",
                None,
            ),
            (
                "def t():\n    result = @entry() or 1\n    assert result == 0\n",
                None,
            ),
            (
                "def t():\n    result = await @entry()\n    assert result == 0\n",
                None,
            ),
            ("result = @entry()\nassert result == 0\n", None),
            (
                "class T:\n    result = @entry()\n    assert result == 0\n",
                None,
            ),
        ] {
            let expected =
                expected.map(|units| units.into_iter().map(str::to_owned).collect::<Vec<_>>());
            assert_eq!(read_units(&int, marked), expected, "{marked}");
        }

        // async 関数の戻り値は await して受けたときだけ得られる。
        let awaited = contract(&blobs, ReturnType::Int, true);
        assert_eq!(
            read_units(
                &awaited,
                "async def t():\n    result = await @entry()\n    assert result == 0\n"
            ),
            Some(vec!["assert result == 0".to_owned()])
        );
        assert_eq!(
            read_units(
                &awaited,
                "async def t():\n    result = @entry()\n    assert result == 0\n"
            ),
            None
        );

        for (returns, literal, accepted) in [
            (ReturnType::Float, "0", true),
            (ReturnType::Float, "-0.5", true),
            (ReturnType::Int, "0.5", false),
            (ReturnType::Str, "'a' \"b\"", true),
            (ReturnType::Str, "f'{x}'", true),
            (ReturnType::Str, "b'a'", false),
            (ReturnType::Bytes, "rb'a' b'b'", true),
            (ReturnType::Bytes, "b'a' 'b'", false),
            (ReturnType::Bool, "True", true),
            (ReturnType::Bool, "1", false),
            (ReturnType::NoneType, "None", true),
            (ReturnType::NoneType, "0", false),
        ] {
            let marked =
                format!("def t():\n    result = @entry()\n    assert result == {literal}\n");
            assert_eq!(
                read_units(&contract(&blobs, returns, false), &marked).is_some(),
                accepted,
                "{returns:?} / {literal}"
            );
        }
    }

    /// 旧版との比較はトークン列で行い、コメント・行継続・空白の違いでは書き換えとみなさない。
    /// 文字列の中の空白とリテラルの綴りは値の違いなので区別する。
    #[test]
    fn statement_units_ignore_comments_and_layout_but_keep_literal_spellings() {
        let key = |source: &str| {
            let tree = parser::parse_source(source.as_bytes(), LangId::Python).unwrap();
            assert!(!tree.root_node().has_error(), "{source}");
            let statement = tree.root_node().named_child(0).unwrap();
            token_key(statement, source.as_bytes()).unwrap()
        };
        let plain = key("assert result == 0\n");
        assert_eq!(plain, key("assert result==0  # 終了コード\n"));
        assert_eq!(plain, key("assert result \\\n    == 0\n"));
        assert_ne!(
            key("assert result == 'a b'\n"),
            key("assert result == 'a  b'\n")
        );
        assert_ne!(plain, key("assert result == 00\n"));
        assert_ne!(plain, key("assert (result == 0)\n"));
    }

    /// 注釈の名前を束縛し得る位置を広く数え、組み込み型と言い切れないものは扱わない。
    #[test]
    fn return_annotation_names_bound_anywhere_in_the_module_are_not_trusted() {
        for (source, rebinds) in [
            ("def entry() -> int:\n    return int('1')\n", false),
            (
                "def entry(value: int) -> int:\n    return isinstance(value, int)\n",
                false,
            ),
            ("def entry() -> int:\n    return obj.int(int=1)\n", false),
            ("int = str\n", true),
            ("from numbers import int\n", true),
            ("import int\n", true),
            ("from helpers import *\n", true),
            ("class int:\n    pass\n", true),
            ("def f():\n    global int\n", true),
            ("for int in range(3):\n    pass\n", true),
            ("def f(int: str):\n    pass\n", true),
            ("def f(int=1):\n    pass\n", true),
            ("with open('x') as int:\n    pass\n", true),
            ("try:\n    pass\nexcept E as int:\n    pass\n", true),
            ("if (int := 1):\n    pass\n", true),
            ("match value:\n    case int:\n        pass\n", true),
            ("first, int = pair\n", true),
        ] {
            let tree = parser::parse_source(source.as_bytes(), LangId::Python).unwrap();
            assert!(!tree.root_node().has_error(), "{source}");
            assert_eq!(
                may_rebind(tree.root_node(), source.as_bytes(), "int"),
                rebinds,
                "{source}"
            );
        }
    }

    /// generator は呼び出しの結果が注釈の型の値でない。入れ子の関数の yield は数えない。
    #[test]
    fn generators_are_detected_outside_nested_scopes_only() {
        for (source, generator) in [
            ("def entry() -> int:\n    return 0\n", false),
            ("def entry() -> int:\n    yield 0\n", true),
            ("def entry() -> int:\n    x = yield\n    return x\n", true),
            (
                "def entry() -> int:\n    def inner():\n        yield 0\n    return 0\n",
                false,
            ),
            (
                "def entry() -> int:\n    f = lambda: (yield)\n    return 0\n",
                false,
            ),
        ] {
            let tree = parser::parse_source(source.as_bytes(), LangId::Python).unwrap();
            assert!(!tree.root_node().has_error(), "{source}");
            let function = tree.root_node().named_child(0).unwrap();
            assert_eq!(is_generator(function), generator, "{source}");
        }
    }
}
