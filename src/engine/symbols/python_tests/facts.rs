//! 1 つの Python モジュールから、テストの収集の判定に使う事実を集める。

use std::collections::{HashMap, HashSet};

use tree_sitter::Node;

use super::{MODULE_HOOKS, Runner, UNITTEST_ROOT_BASES, UNITTEST_ROOT_NAMES};
use crate::engine::symbols::{PythonModuleExportPolicy, python_module_export_policy};

/// クラス自身の名前空間での `__init__` / `__new__` の束縛。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum OwnBinding {
    /// 名前空間に現れない。
    Absent,
    /// 本体の直下にあるデコレータの無い `def` の名前としてだけ現れる。
    Def,
    /// それ以外の形 (代入・条件付きの def・デコレータ付きの def・import・del など) で現れる。
    Unknown,
}

impl OwnBinding {
    fn observe(&mut self, plain_def: bool) {
        *self = match (*self, plain_def) {
            (Self::Absent | Self::Def, true) => Self::Def,
            _ => Self::Unknown,
        };
    }
}

/// 式から読み取る入れ子の深さの上限。超えた部分は何でもありうるとする (作る値と、それを解決する
/// 再帰の深さを抑える)。
const MAX_EXPRESSION_DEPTH: usize = 16;

/// クラスの基底や、名前に束縛した式。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Base {
    /// 識別子 (`Base`)。
    Name(String),
    /// 属性 (`helpers.Base` → `["helpers", "Base"]`) の要素の並び。
    Qualified(Vec<String>),
    /// subscript (`Base[int]`、`registry["x"]`) の値の式。値がクラスなら、標準の generic は
    /// 基底をそのクラスにする。独自の `__class_getitem__` や metaclass の `__getitem__` は何でも
    /// 返しうる。値が入れ物なら、要素は実行時に決まる。
    Subscript(Box<Base>),
    /// 呼び出し (`f(A, B)`) の呼ぶものと引数 (`*` / `**` の展開は要素)。
    Call {
        callee: Box<Base>,
        arguments: Vec<Base>,
    },
    /// どれか 1 つ (`A if c else B`、`A or B`)。
    Union(Vec<Base>),
    /// リスト・タプル・集合の表示と内包表記、`type` 文の値。それ自体はクラスでない。表示なら
    /// 要素を持つ (`None` は内包表記などで、要素が静的に分からない)。`exact` は、`*` の展開を
    /// 含まず、順序と要素の数が実行時と同じか。`mutable` は、あとで要素を足しうるか (タプル
    /// でない)。
    Container {
        elements: Option<Vec<Base>>,
        exact: bool,
        mutable: bool,
    },
    /// 辞書の表示。反復とアンパックはキーを、`**` の展開は値を取り出す。
    Mapping { keys: Vec<Base>, values: Vec<Base> },
    /// 値の要素 (アンパックの代入の先、`for` の変数、`*` の展開)。
    Item(Box<Base>),
    /// 辞書の値 (`**` の展開)。
    Values(Box<Base>),
    /// 演算 (`a | b`、`-a`、`a == b`) の被演算子。演算子の多重定義は何でも返しうる。
    Operator(Vec<Base>),
    /// lambda。
    Function,
    /// クラスにも、クラスを持つ入れ物にもならない値 (数・文字列・`not` の結果)。
    NotClass,
    /// 何に束縛するかが式から決まらないもの (`with` / `except` の `as`・`match` の capture・
    /// 累算代入・上のどれにも当たらない式)。どのクラスでもありうる。
    Other,
}

/// [`Base::from_expression`] の明示スタックの 1 段。
enum Frame<'t> {
    /// 式のノードと、その入れ子の深さ。
    Enter(Node<'t>, usize),
    /// 直前の値を subscript の値にする。
    Subscript,
    /// 直前の (1 + 引数の数) 個の値を、呼ぶものと引数にする。
    Call(usize),
    /// 直前の個数の値を、どれか 1 つにする。
    Union(usize),
    /// 直前の個数の値を、表示の要素にする (`*` の展開を含まず順序があるか, 要素を足しうるか)。
    Container(usize, bool, bool),
    /// 直前の (キーの数 + 値の数) 個の値を、辞書の表示にする。
    Mapping(usize, usize),
    /// 直前の個数の値を、演算の被演算子にする。
    Operator(usize),
    /// 直前の値の要素にする。
    Item,
    /// 直前の値の、辞書の値にする。
    Values,
}

/// 名前に束縛する、要素の分からない入れ物 (`type` 文の値や `*rest`)。
pub(super) const OPAQUE: Base = Base::Container {
    elements: None,
    exact: false,
    mutable: true,
};

impl Base {
    /// 式を読み取る。構文木は明示スタックで辿り、[`MAX_EXPRESSION_DEPTH`] より深い部分は
    /// [`Base::Other`] にする。
    pub(super) fn from_expression(node: Node<'_>, source: &[u8]) -> Self {
        let mut frames = vec![Frame::Enter(node, 0)];
        let mut values: Vec<Self> = Vec::new();
        while let Some(frame) = frames.pop() {
            let (node, depth) = match frame {
                Frame::Enter(node, depth) => (node, depth),
                Frame::Subscript => {
                    let value = values.pop().unwrap_or(Self::Other);
                    values.push(match value {
                        Self::Name(_) | Self::Qualified(_) | Self::Subscript(_) => {
                            Self::Subscript(Box::new(value))
                        }
                        _ => Self::Other,
                    });
                    continue;
                }
                Frame::Call(count) => {
                    let arguments = values.split_off(values.len().saturating_sub(count));
                    let callee = values.pop().unwrap_or(Self::Other);
                    values.push(Self::Call {
                        callee: Box::new(callee),
                        arguments,
                    });
                    continue;
                }
                Frame::Union(count) => {
                    let alternatives = values.split_off(values.len().saturating_sub(count));
                    values.push(Self::Union(alternatives));
                    continue;
                }
                Frame::Container(count, exact, mutable) => {
                    let elements = values.split_off(values.len().saturating_sub(count));
                    values.push(Self::Container {
                        elements: Some(elements),
                        exact,
                        mutable,
                    });
                    continue;
                }
                Frame::Mapping(key_count, value_count) => {
                    let entries = values.split_off(values.len().saturating_sub(value_count));
                    let keys = values.split_off(values.len().saturating_sub(key_count));
                    values.push(Self::Mapping {
                        keys,
                        values: entries,
                    });
                    continue;
                }
                Frame::Operator(count) => {
                    let operands = values.split_off(values.len().saturating_sub(count));
                    values.push(Self::Operator(operands));
                    continue;
                }
                Frame::Item => {
                    let value = values.pop().unwrap_or(Self::Other);
                    values.push(value.item());
                    continue;
                }
                Frame::Values => {
                    let value = values.pop().unwrap_or(Self::Other);
                    values.push(value.mapping_values());
                    continue;
                }
            };
            if depth > MAX_EXPRESSION_DEPTH {
                values.push(Self::Other);
                continue;
            }
            let next = depth + 1;
            match node.kind() {
                "identifier" => values.push(Self::Name(node_text(node, source).to_string())),
                "attribute" => {
                    values.push(dotted_path(node, source).map_or(Self::Other, Self::Qualified));
                }
                "subscript" => match node.child_by_field_name("value") {
                    Some(value) => {
                        frames.push(Frame::Subscript);
                        enter_children(&mut frames, &[value], next);
                    }
                    None => values.push(Self::Other),
                },
                "call" => {
                    let Some(function) = node.child_by_field_name("function") else {
                        values.push(Self::Other);
                        continue;
                    };
                    // 引数が 1 つの generator 式 (`f(x for x in y)`) なら、それが引数。
                    let arguments: Vec<Node<'_>> = match node.child_by_field_name("arguments") {
                        Some(list) if list.kind() == "argument_list" => expression_children(list),
                        Some(argument) => vec![argument],
                        None => Vec::new(),
                    };
                    frames.push(Frame::Call(arguments.len()));
                    let mut children = vec![function];
                    children.extend(arguments);
                    enter_children(&mut frames, &children, next);
                }
                // 値を持つ子へ。
                "keyword_argument" | "named_expression" => {
                    match node.child_by_field_name("value") {
                        Some(value) => enter_children(&mut frames, &[value], next),
                        None => values.push(Self::Other),
                    }
                }
                "parenthesized_expression" => match expression_children(node).as_slice() {
                    [inner] => enter_children(&mut frames, &[*inner], next),
                    _ => values.push(Self::Other),
                },
                // `*x` は要素、`**x` は辞書の値 (引数の展開)。
                "list_splat" | "dictionary_splat" => match expression_children(node).as_slice() {
                    [inner] => {
                        frames.push(if node.kind() == "list_splat" {
                            Frame::Item
                        } else {
                            Frame::Values
                        });
                        enter_children(&mut frames, &[*inner], next);
                    }
                    _ => values.push(Self::Other),
                },
                "tuple" | "list" | "set" | "expression_list" => {
                    let children = expression_children(node);
                    // 集合は順序を持たないので、位置で対応させない。リストと集合は、あとで要素を
                    // 足しうる。
                    let exact = node.kind() != "set"
                        && !children.iter().any(|child| child.kind() == "list_splat");
                    let mutable = matches!(node.kind(), "list" | "set");
                    frames.push(Frame::Container(children.len(), exact, mutable));
                    enter_children(&mut frames, &children, next);
                }
                "dictionary" => {
                    // キーと値 (`**x` は x のキーと値)。キーは (式, `**x` の x か)。
                    let mut keys: Vec<(Node<'_>, bool)> = Vec::new();
                    let mut entries: Vec<Node<'_>> = Vec::new();
                    for child in expression_children(node) {
                        match child.kind() {
                            "pair" => {
                                if let (Some(key), Some(value)) = (
                                    child.child_by_field_name("key"),
                                    child.child_by_field_name("value"),
                                ) {
                                    keys.push((key, false));
                                    entries.push(value);
                                }
                            }
                            "dictionary_splat" => {
                                if let [inner] = expression_children(child).as_slice() {
                                    keys.push((*inner, true));
                                    entries.push(child);
                                }
                            }
                            _ => {}
                        }
                    }
                    frames.push(Frame::Mapping(keys.len(), entries.len()));
                    enter_children(&mut frames, &entries, next);
                    // キーは値より先に処理する。`**x` のキーは x の要素。
                    for (key, splat) in keys.into_iter().rev() {
                        if splat {
                            frames.push(Frame::Item);
                        }
                        frames.push(Frame::Enter(key, next));
                    }
                }
                "list_comprehension"
                | "set_comprehension"
                | "dictionary_comprehension"
                | "generator_expression" => values.push(OPAQUE),
                "conditional_expression" => match expression_children(node).as_slice() {
                    // `A if condition else B`: 名前付きの子は (A, condition, B)。
                    [consequence, _, alternative] => {
                        frames.push(Frame::Union(2));
                        enter_children(&mut frames, &[*consequence, *alternative], next);
                    }
                    _ => values.push(Self::Other),
                },
                // `A or B` / `A and B` は、どちらかの値。
                "boolean_operator" => match (
                    node.child_by_field_name("left"),
                    node.child_by_field_name("right"),
                ) {
                    (Some(left), Some(right)) => {
                        frames.push(Frame::Union(2));
                        enter_children(&mut frames, &[left, right], next);
                    }
                    _ => values.push(Self::Other),
                },
                // `is` / `in` (と否定) だけの比較は bool を返す。
                "comparison_operator" if only_boolean_comparisons(node) => {
                    values.push(Self::NotClass);
                }
                // 演算子は多重定義 (`__or__` など) で何でも返しうる。被演算子で決める。
                "binary_operator" | "unary_operator" | "comparison_operator" => {
                    let operands = expression_children(node);
                    frames.push(Frame::Operator(operands.len()));
                    enter_children(&mut frames, &operands, next);
                }
                "lambda" => values.push(Self::Function),
                "integer"
                | "float"
                | "string"
                | "concatenated_string"
                | "true"
                | "false"
                | "none"
                | "ellipsis"
                | "not_operator" => values.push(Self::NotClass),
                _ => values.push(Self::Other),
            }
        }
        values.pop().unwrap_or(Self::Other)
    }

    /// この値の要素 (アンパックや反復で取り出すもの)。表示なら要素のどれか (辞書はキー)。
    fn item(self) -> Self {
        match self {
            Self::Container {
                elements: Some(elements),
                ..
            } => Self::Union(elements),
            Self::Mapping { keys, .. } => Self::Union(keys),
            Self::Union(alternatives) => {
                Self::Union(alternatives.into_iter().map(Self::item).collect())
            }
            Self::NotClass => Self::NotClass,
            other => Self::Item(Box::new(other)),
        }
    }

    /// この値を `**` で展開したときの値。辞書の表示なら値のどれか。
    fn mapping_values(self) -> Self {
        match self {
            Self::Mapping { values, .. } => Self::Union(values),
            Self::Union(alternatives) => {
                Self::Union(alternatives.into_iter().map(Self::mapping_values).collect())
            }
            other => Self::Values(Box::new(other)),
        }
    }

    /// クラスもプロジェクトの値も持たない値か (数・文字列と、それだけのタプルの表示
    /// `("x", ("y", 1))`)。リスト・集合・辞書は、あとでクラスを足すレジストリでありうるので
    /// 含めない。
    pub(super) fn is_scalar(&self) -> bool {
        match self {
            Self::NotClass => true,
            Self::Container {
                elements: Some(elements),
                mutable: false,
                ..
            } => elements.iter().all(Self::is_scalar),
            _ => false,
        }
    }

    /// 束縛を問わない unittest の root の書き方 (`TestCase` / `unittest.TestCase` など) か。
    pub(super) fn is_unittest_root_text(&self) -> bool {
        match self {
            Self::Name(name) => UNITTEST_ROOT_BASES.contains(&name.as_str()),
            Self::Qualified(path) => UNITTEST_ROOT_BASES.contains(&path.join(".").as_str()),
            _ => false,
        }
    }
}

/// import の対象のモジュール (`from ..pkg import x` は `level` = 2、`path` = `["pkg"]`)。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct ModuleRef {
    pub(super) level: usize,
    pub(super) path: Vec<String>,
}

impl ModuleRef {
    /// 標準ライブラリの `unittest` (とその下のモジュール) か。
    pub(super) fn is_unittest(&self) -> bool {
        self.level == 0 && self.path.first().is_some_and(|first| first == "unittest")
    }

    /// このモジュールの下のモジュール `name`。
    pub(super) fn child(&self, name: &str) -> Self {
        let mut child = self.clone();
        child.path.push(name.to_string());
        child
    }
}

/// import による名前の束縛。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Import {
    /// `import a.b` は `a` を `a` に、`import a.b as m` は `m` を `a.b` に束縛する。
    Module(ModuleRef),
    /// `from M import name [as alias]`。
    From { module: ModuleRef, name: String },
}

impl Import {
    /// `unittest` の TestCase を指すか。
    pub(super) fn is_unittest_root(&self) -> bool {
        matches!(self, Self::From { module, name }
            if module.is_unittest() && UNITTEST_ROOT_NAMES.contains(&name.as_str()))
    }

    /// `unittest` (とその下) のモジュールを指すか。
    pub(super) fn is_unittest_module(&self) -> bool {
        match self {
            Self::Module(module) | Self::From { module, .. } => module.is_unittest(),
        }
    }

    /// 束縛した名前がモジュールとして指しうるもの (`from p import a` はサブモジュール `p.a`)。
    pub(super) fn as_module(&self) -> ModuleRef {
        match self {
            Self::Module(module) => module.clone(),
            Self::From { module, name } => module.child(name),
        }
    }
}

/// 1 つの名前空間 (module か、クラスの本体) の束縛。同じ名前の束縛は上書きせず和集合で持つ
/// (クラス文を実行する時点の束縛を厳密には扱わない)。
#[derive(Debug, Default)]
pub(super) struct Scope {
    /// 名前 → この名前空間で定義したクラスの添字。
    classes: HashMap<String, Vec<usize>>,
    /// 名前 → この名前空間での別名の右辺。
    aliases: HashMap<String, Vec<Base>>,
    /// 名前 → この名前空間での import。
    imports: HashMap<String, Vec<Import>>,
    /// この名前空間で定義した関数 (`def` と lambda の束縛) の名前。
    functions: HashSet<String>,
}

impl Scope {
    fn names(&self) -> impl Iterator<Item = &str> {
        self.classes
            .keys()
            .chain(self.aliases.keys())
            .chain(self.imports.keys())
            .chain(self.functions.iter())
            .map(String::as_str)
    }
}

/// 名前空間の 1 つの名前への束縛。
pub(super) struct Bindings<'f> {
    pub(super) classes: &'f [usize],
    pub(super) aliases: &'f [Base],
    pub(super) imports: &'f [Import],
    /// 関数 (`def` か lambda の束縛) にも束縛される。
    pub(super) function: bool,
}

impl Bindings<'_> {
    pub(super) fn is_empty(&self) -> bool {
        self.classes.is_empty()
            && self.aliases.is_empty()
            && self.imports.is_empty()
            && !self.function
    }
}

#[derive(Debug)]
pub(super) struct ClassFacts {
    pub(super) name: String,
    pub(super) start: (usize, usize),
    /// 本体の中で定義したクラスの添字。module 直下なら `None`。
    pub(super) owner: Option<usize>,
    pub(super) decorated: bool,
    /// クラス文にキーワード引数 (`metaclass=...` や `__init_subclass__` への引数) がある。
    pub(super) keyword_arguments: bool,
    /// クラス文で metaclass を指定しうる (`metaclass=` か `**` の展開)。
    pub(super) metaclass: bool,
    pub(super) bases: Vec<Base>,
    pub(super) init: OwnBinding,
    pub(super) new: OwnBinding,
    /// 名前空間に `__test__ = True` がある。
    pub(super) test_marker: bool,
    /// 名前空間の `test*` と、テストランナーが名前で呼ぶメソッドの (名前, 開始位置)。
    pub(super) members: Vec<(String, (usize, usize))>,
    /// io の ABC の契約になりうるメソッドとプロパティの定義。
    pub(super) io_members: Vec<(String, (usize, usize))>,
    /// 名前空間の直下で定義したクラスの添字。
    pub(super) nested: Vec<usize>,
    /// 名前空間の別名の (名前, 右辺)。
    pub(super) aliases: Vec<(String, Base)>,
    /// 名前空間の import の (束縛の名前, import)。
    pub(super) imports: Vec<(String, Import)>,
    /// 本体で独自の `__class_getitem__` を束縛し、subscript が任意のクラスを返しうる。
    pub(super) class_getitem: bool,
    /// 名前空間の束縛。
    pub(super) scope: Scope,
}

impl ClassFacts {
    fn observe_constructor(&mut self, name: &str, plain_def: bool) {
        match name {
            "__init__" => self.init.observe(plain_def),
            "__new__" => self.new.observe(plain_def),
            _ => {}
        }
    }

    /// 本体で `name` を (定義・代入・import のどれかで) 束縛した。
    fn observe_member(&mut self, name: &str) {
        if name == "__class_getitem__" {
            self.class_getitem = true;
        }
    }
}

/// 1 モジュール分の事実。
#[derive(Debug)]
pub(crate) struct PythonModuleFacts {
    /// 関数・lambda の本体の外にあるクラス定義。
    pub(super) classes: Vec<ClassFacts>,
    /// module 直下の束縛。
    module_scope: Scope,
    /// module 直下の別名の (名前, 右辺)。
    pub(super) aliases: Vec<(String, Base)>,
    /// module 直下の import の (束縛の名前, import)。
    pub(super) imports: Vec<(String, Import)>,
    /// module 直下の `from M import *`。
    pub(super) star_imports: Vec<ModuleRef>,
    /// module 直下に `def __getattr__` があり、静的に無い属性を実行時に作りうる。
    pub(super) module_getattr: bool,
    /// `from この モジュール import *` で入る名前の規約 (`__all__` と `_` の規約)。
    export_policy: PythonModuleExportPolicy,
    /// クラスの外から `__init__` / `__new__` に触れうる (module 直下の出現か、
    /// クラスの名前空間の `X.__init__`)。どのクラスについても飛ばすとは言わない。
    constructor_rewritten: bool,
    /// module 直下で `object` が束縛されうる (識別子の出現、同じ名前のクラス、star import)。
    object_rebound: bool,
    /// module 直下の `test*` と、pytest が名前で呼ぶ関数の開始位置。
    pub(super) functions: Vec<(usize, usize)>,
    /// 構文エラーがあり、エラー回復の形によっては定義や束縛を取りこぼしうる。
    pub(crate) has_error: bool,
    /// 本体で `__class_getitem__ = classmethod(X)` と書いたクラスの (添字, `X` の書き方)。
    /// `X` が `types` の `GenericAlias` なら標準の generic と同じ (束縛は走査の後で確かめる)。
    standard_getitem_candidates: Vec<(usize, String)>,
}

/// 名前空間の走査で訪れるノード。
#[derive(Clone, Copy)]
struct Visit<'t> {
    node: Node<'t>,
    /// クラスの本体の直下の文か。
    in_body: bool,
    /// `decorated_definition` の子か。
    decorated: bool,
}

impl<'t> Visit<'t> {
    fn plain(node: Node<'t>) -> Self {
        Self {
            node,
            in_body: false,
            decorated: false,
        }
    }
}

impl PythonModuleFacts {
    /// 構文木を明示スタックで辿る。関数と lambda の本体には入らない (そこで定義したものは
    /// モジュールやクラスの属性にならず、テストランナーも集めない)。
    pub(crate) fn collect(root: Node<'_>, source: &[u8]) -> Self {
        let mut facts = Self {
            classes: Vec::new(),
            module_scope: Scope::default(),
            aliases: Vec::new(),
            imports: Vec::new(),
            star_imports: Vec::new(),
            module_getattr: false,
            export_policy: python_module_export_policy(root, source),
            constructor_rewritten: false,
            object_rebound: false,
            functions: Vec::new(),
            has_error: root.has_error(),
            standard_getitem_candidates: Vec::new(),
        };
        // (名前空間を持つクラスの添字 (module は None), 名前空間の文を子に持つノード)
        let mut scopes: Vec<(Option<usize>, Node<'_>)> = vec![(None, root)];
        while let Some((scope, statements)) = scopes.pop() {
            let mut stack: Vec<Visit<'_>> = Vec::new();
            push_children(&mut stack, statements, scope.is_some(), false);
            while let Some(visit) = stack.pop() {
                let node = visit.node;
                match node.kind() {
                    "class_definition" => {
                        let index = facts.add_class(node, source, scope, visit.decorated);
                        // 本体と、エラー回復で本体の文を入れた ERROR の子 (シンボルの抽出は、
                        // そこにあるメソッドもクラスのメンバーにする)。
                        let body = node.child_by_field_name("body");
                        for child in expression_children(node) {
                            if child.is_error() || Some(child) == body {
                                scopes.push((Some(index), child));
                            }
                        }
                    }
                    "function_definition" => {
                        if let Some(name) = definition_name(node, source) {
                            facts.add_function(
                                scope,
                                name,
                                start_of(node),
                                visit.in_body && !visit.decorated,
                            );
                        }
                    }
                    "decorated_definition" => {
                        push_children(&mut stack, node, visit.in_body, true);
                    }
                    // lambda の本体は式なので、定義も束縛も無い。
                    "lambda" => {}
                    "assignment" => {
                        facts.observe_assignment(scope, node, source);
                        push_children(&mut stack, node, false, false);
                    }
                    // `for X in iterable` の `X` は iterable の要素。
                    "for_statement" => {
                        if let Some(left) = node.child_by_field_name("left") {
                            let iterable = node
                                .child_by_field_name("right")
                                .map_or(Base::Other, |right| Base::from_expression(right, source));
                            facts.bind_targets(scope, left, source, iterable.item());
                        }
                        push_children(&mut stack, node, false, false);
                    }
                    // `G += 1` は、何に束縛するかが式で決まらない。
                    "augmented_assignment" => {
                        if let Some(left) = node.child_by_field_name("left") {
                            facts.bind_targets(scope, left, source, Base::Other);
                        }
                        push_children(&mut stack, node, false, false);
                    }
                    // `type X = ...` はクラスでない値だが、`__value__` で右辺を取り出せる。
                    "type_alias_statement" => {
                        if let Some(left) = node.child_by_field_name("left") {
                            facts.bind_targets(scope, left, source, OPAQUE);
                        }
                        push_children(&mut stack, node, false, false);
                    }
                    // `with ... as X`・`except ... as X`。
                    "as_pattern_target" => {
                        facts.bind_targets(scope, node, source, Base::Other);
                        push_children(&mut stack, node, false, false);
                    }
                    "named_expression" => {
                        if let Some(name) = node.child_by_field_name("name") {
                            let value = node
                                .child_by_field_name("value")
                                .map_or(Base::Other, |value| Base::from_expression(value, source));
                            facts.bind_targets(scope, name, source, value);
                        }
                        push_children(&mut stack, node, false, false);
                    }
                    "case_clause" => {
                        facts.observe_case(scope, node, source);
                        push_children(&mut stack, node, false, false);
                    }
                    "import_statement" => {
                        facts.observe_import(scope, node, source);
                        push_children(&mut stack, node, false, false);
                    }
                    "import_from_statement" => {
                        facts.observe_import_from(scope, node, source);
                        push_children(&mut stack, node, false, false);
                    }
                    "attribute" => {
                        // `X.__init__` は、クラスの外からコンストラクタを書き換えうる。
                        if node
                            .child_by_field_name("attribute")
                            .is_some_and(|tail| is_constructor_name(node_text(tail, source)))
                        {
                            facts.constructor_rewritten = true;
                        }
                        if let Some(object) = node.child_by_field_name("object") {
                            stack.push(Visit::plain(object));
                        }
                    }
                    "identifier" => facts.observe_identifier(scope, node_text(node, source)),
                    "wildcard_import" if scope.is_none() => facts.object_rebound = true,
                    _ => push_children(&mut stack, node, false, false),
                }
            }
        }
        facts.finish_standard_getitems();
        facts
    }

    fn scope_mut(&mut self, scope: Option<usize>) -> &mut Scope {
        match scope {
            None => &mut self.module_scope,
            Some(owner) => &mut self.classes[owner].scope,
        }
    }

    fn add_class(
        &mut self,
        node: Node<'_>,
        source: &[u8],
        owner: Option<usize>,
        decorated: bool,
    ) -> usize {
        let index = self.classes.len();
        let name = definition_name(node, source)
            .unwrap_or_default()
            .to_string();
        let mut bases = Vec::new();
        let mut keyword_arguments = false;
        let mut metaclass = false;
        if let Some(arguments) = node.child_by_field_name("superclasses") {
            for argument in expression_children(arguments) {
                match argument.kind() {
                    "keyword_argument" => {
                        keyword_arguments = true;
                        metaclass |= argument
                            .child_by_field_name("name")
                            .is_some_and(|name| node_text(name, source) == "metaclass");
                    }
                    "dictionary_splat" => {
                        keyword_arguments = true;
                        metaclass = true;
                    }
                    _ => bases.push(Base::from_expression(argument, source)),
                }
            }
        }
        match owner {
            None => self.observe_identifier(None, &name),
            Some(owner) => {
                let class = &mut self.classes[owner];
                class.nested.push(index);
                class.observe_constructor(&name, false);
                class.observe_member(&name);
            }
        }
        self.scope_mut(owner)
            .classes
            .entry(name.clone())
            .or_default()
            .push(index);
        self.classes.push(ClassFacts {
            name,
            start: start_of(node),
            owner,
            decorated,
            keyword_arguments,
            metaclass,
            bases,
            init: OwnBinding::Absent,
            new: OwnBinding::Absent,
            test_marker: false,
            members: Vec::new(),
            io_members: Vec::new(),
            nested: Vec::new(),
            aliases: Vec::new(),
            imports: Vec::new(),
            class_getitem: false,
            scope: Scope::default(),
        });
        index
    }

    /// `def` か lambda の束縛を 1 つ記録する。`plain_def` は、クラスの本体の直下にある
    /// デコレータの無い `def` か。
    fn add_function(
        &mut self,
        scope: Option<usize>,
        name: &str,
        start: (usize, usize),
        plain_def: bool,
    ) {
        self.scope_mut(scope).functions.insert(name.to_string());
        match scope {
            None => {
                self.observe_identifier(None, name);
                if name == "__getattr__" {
                    self.module_getattr = true;
                }
                if name.starts_with("test") || MODULE_HOOKS.contains(&name) {
                    self.functions.push(start);
                }
            }
            Some(owner) => {
                let class = &mut self.classes[owner];
                class.observe_member(name);
                class.observe_constructor(name, plain_def);
                if Runner::Pytest.calls(name) || Runner::Unittest.calls(name) {
                    class.members.push((name.to_string(), start));
                }
                if super::is_io_protocol_name(name) {
                    class.io_members.push((name.to_string(), start));
                }
            }
        }
    }

    fn observe_assignment(&mut self, scope: Option<usize>, node: Node<'_>, source: &[u8]) {
        let Some(left) = node.child_by_field_name("left") else {
            return;
        };
        // 注釈だけの `X: int` は名前を束縛しない。
        let Some(mut right) = node.child_by_field_name("right") else {
            return;
        };
        // `A = B = value` は、どちらも value に束縛する (`B = value` は入れ子の代入として見る)。
        while right.kind() == "assignment" {
            match right.child_by_field_name("right") {
                Some(value) => right = value,
                None => return,
            }
        }
        if left.kind() != "identifier" {
            // `A, B = ...` の名前は、右辺の要素に束縛する。
            let value = Base::from_expression(right, source);
            self.bind_targets(scope, left, source, value);
            return;
        }
        let name = node_text(left, source);
        match right.kind() {
            // シンボルの抽出と同じく、識別子に lambda を束縛した代入を関数として扱う。
            "lambda" => self.add_function(scope, name, start_of(node), false),
            "true" if name == "__test__" => {
                if let Some(owner) = scope {
                    self.classes[owner].test_marker = true;
                }
            }
            _ => {
                if let Some(owner) = scope {
                    let standard = if name == "__class_getitem__" {
                        standard_getitem_argument(right, source)
                    } else {
                        None
                    };
                    match standard {
                        Some(argument) => self
                            .standard_getitem_candidates
                            .push((owner, argument.to_string())),
                        None => self.classes[owner].observe_member(name),
                    }
                }
                self.bind_alias(scope, name, Base::from_expression(right, source));
            }
        }
    }

    fn bind_alias(&mut self, scope: Option<usize>, name: &str, target: Base) {
        let alias = (name.to_string(), target.clone());
        match scope {
            None => self.aliases.push(alias),
            Some(owner) => self.classes[owner].aliases.push(alias),
        }
        self.scope_mut(scope)
            .aliases
            .entry(name.to_string())
            .or_default()
            .push(target);
    }

    /// `target` (代入の左辺・`for` の変数・`as` の先など) の名前を `value` に束縛する。
    /// アンパック (`A, (B, C) = ...`) の名前は、入れ子の段ごとに要素に束縛する (右辺が `*` の
    /// 展開を含まない同じ数の要素のリストかタプルの表示なら位置で対応させ、そうでなければ要素の
    /// どれか。`*rest` はリスト)。属性や subscript への代入は名前を束縛しないので見ない。
    fn bind_targets(&mut self, scope: Option<usize>, target: Node<'_>, source: &[u8], value: Base) {
        let mut stack = vec![(target, value, 0usize)];
        while let Some((node, value, depth)) = stack.pop() {
            match node.kind() {
                "identifier" => {
                    let name = node_text(node, source);
                    if let Some(owner) = scope {
                        self.classes[owner].observe_member(name);
                    }
                    self.bind_alias(scope, name, value);
                }
                "attribute" | "subscript" => {}
                "pattern_list" | "tuple_pattern" | "list_pattern" | "tuple" | "list"
                | "expression_list" => {
                    let children = expression_children(node);
                    let starred = children
                        .iter()
                        .any(|child| matches!(child.kind(), "list_splat_pattern" | "list_splat"));
                    match value {
                        Base::Container {
                            elements: Some(elements),
                            exact: true,
                            ..
                        } if !starred
                            && elements.len() == children.len()
                            && depth < MAX_EXPRESSION_DEPTH =>
                        {
                            stack.extend(
                                children
                                    .into_iter()
                                    .zip(elements)
                                    .map(|(child, element)| (child, element, depth + 1)),
                            );
                        }
                        value => {
                            let item = if depth >= MAX_EXPRESSION_DEPTH {
                                Base::Other
                            } else {
                                value.item()
                            };
                            stack.extend(
                                children
                                    .into_iter()
                                    .map(|child| (child, item.clone(), depth + 1)),
                            );
                        }
                    }
                }
                "list_splat_pattern" | "list_splat" => stack.extend(
                    expression_children(node)
                        .into_iter()
                        .map(|child| (child, OPAQUE, depth + 1)),
                ),
                _ => stack.extend(
                    expression_children(node)
                        .into_iter()
                        .map(|child| (child, value.clone(), depth + 1)),
                ),
            }
        }
    }

    /// `match` の `case` の capture (`case [Parent]`、`case Point(x=Inner) as Whole`) は、
    /// 何でもありうるものに束縛する。class pattern のクラス名、キーワード、`.` を含む値の
    /// pattern は束縛ではない。
    fn observe_case(&mut self, scope: Option<usize>, case: Node<'_>, source: &[u8]) {
        let mut stack: Vec<Node<'_>> = expression_children(case)
            .into_iter()
            .filter(|child| child.kind() == "case_pattern")
            .collect();
        let mut captures: Vec<String> = Vec::new();
        while let Some(node) = stack.pop() {
            let children = expression_children(node);
            match node.kind() {
                "dotted_name" => {
                    if let [identifier] = children.as_slice() {
                        captures.push(node_text(*identifier, source).to_string());
                    }
                }
                // 先頭の子はクラス名かキーワード。
                "class_pattern" | "keyword_pattern" => stack.extend(children.into_iter().skip(1)),
                "splat_pattern" | "as_pattern" => {
                    for child in children {
                        if child.kind() == "identifier" {
                            captures.push(node_text(child, source).to_string());
                        } else {
                            stack.push(child);
                        }
                    }
                }
                _ => stack.extend(children),
            }
        }
        for name in captures {
            if let Some(owner) = scope {
                self.classes[owner].observe_member(&name);
            }
            self.bind_alias(scope, &name, Base::Other);
        }
    }

    /// `__class_getitem__ = classmethod(X)` の `X` が `types` の `GenericAlias`
    /// (`from types import GenericAlias` か `import types` の `types.GenericAlias`) で、
    /// `classmethod` もその名前も束縛し直していなければ、標準の generic と同じとする。
    /// そうでなければ独自。
    fn finish_standard_getitems(&mut self) {
        let candidates = std::mem::take(&mut self.standard_getitem_candidates);
        for (class, argument) in candidates {
            let standard = self.star_imports.is_empty()
                && self.bindings(Some(class), "classmethod").is_empty()
                && self.bindings(None, "classmethod").is_empty()
                && match argument.as_str() {
                    "GenericAlias" => {
                        self.bindings(Some(class), "GenericAlias").is_empty()
                            && self.only_imports("GenericAlias", |import| {
                                matches!(import, Import::From { module, name }
                                    if module.level == 0
                                        && module.path == ["types"]
                                        && name == "GenericAlias")
                            })
                    }
                    "types.GenericAlias" => {
                        self.bindings(Some(class), "types").is_empty()
                            && self.only_imports("types", |import| {
                                matches!(import, Import::Module(module)
                                    if module.level == 0 && module.path == ["types"])
                            })
                    }
                    _ => false,
                };
            if !standard {
                self.classes[class].class_getitem = true;
            }
        }
    }

    /// module 直下の `name` の束縛が import だけで、どれも `expected` を満たすか。
    fn only_imports(&self, name: &str, expected: impl Fn(&Import) -> bool) -> bool {
        let bindings = self.bindings(None, name);
        bindings.classes.is_empty()
            && bindings.aliases.is_empty()
            && !bindings.function
            && !bindings.imports.is_empty()
            && bindings.imports.iter().all(expected)
    }

    /// `import a.b` / `import a.b as m`。
    fn observe_import(&mut self, scope: Option<usize>, node: Node<'_>, source: &[u8]) {
        let mut cursor = node.walk();
        for name in node.children_by_field_name("name", &mut cursor) {
            let (path, alias) = imported_name(name, source);
            let Some(path) = path else {
                continue;
            };
            let (bound, module) = match alias {
                Some(alias) => (alias, path),
                None => (path[0].clone(), vec![path[0].clone()]),
            };
            self.bind_import(
                scope,
                bound,
                Import::Module(ModuleRef {
                    level: 0,
                    path: module,
                }),
            );
        }
    }

    /// `from M import a, b as c` / `from M import *`。
    fn observe_import_from(&mut self, scope: Option<usize>, node: Node<'_>, source: &[u8]) {
        let Some(module) = node
            .child_by_field_name("module_name")
            .and_then(|module| module_ref(module, source))
        else {
            return;
        };
        if scope.is_none()
            && expression_children(node)
                .iter()
                .any(|child| child.kind() == "wildcard_import")
        {
            self.star_imports.push(module.clone());
        }
        let mut cursor = node.walk();
        for name in node.children_by_field_name("name", &mut cursor) {
            let (imported, alias) = imported_name(name, source);
            // `from M import a.b` は書けないので、1 要素だけを受け付ける。
            let Some(imported) = imported.and_then(|path| match <[String; 1]>::try_from(path) {
                Ok([name]) => Some(name),
                Err(_) => None,
            }) else {
                continue;
            };
            let bound = alias.unwrap_or_else(|| imported.clone());
            self.bind_import(
                scope,
                bound,
                Import::From {
                    module: module.clone(),
                    name: imported,
                },
            );
        }
    }

    fn bind_import(&mut self, scope: Option<usize>, bound: String, import: Import) {
        match scope {
            None => self.imports.push((bound.clone(), import.clone())),
            Some(owner) => {
                let class = &mut self.classes[owner];
                class.observe_member(&bound);
                class.imports.push((bound.clone(), import.clone()));
            }
        }
        self.scope_mut(scope)
            .imports
            .entry(bound)
            .or_default()
            .push(import);
    }

    fn observe_identifier(&mut self, scope: Option<usize>, name: &str) {
        match scope {
            None => {
                if is_constructor_name(name) {
                    self.constructor_rewritten = true;
                }
                if name == "object" {
                    self.object_rebound = true;
                }
            }
            Some(owner) => self.classes[owner].observe_constructor(name, false),
        }
    }

    /// 名前空間 (`None` は module、`Some(c)` はクラス `c` の本体) の `name` への束縛。
    pub(super) fn bindings(&self, scope: Option<usize>, name: &str) -> Bindings<'_> {
        let scope = match scope {
            None => &self.module_scope,
            Some(class) => &self.classes[class].scope,
        };
        Bindings {
            classes: scope.classes.get(name).map_or(&[], Vec::as_slice),
            aliases: scope.aliases.get(name).map_or(&[], Vec::as_slice),
            imports: scope.imports.get(name).map_or(&[], Vec::as_slice),
            function: scope.functions.contains(name),
        }
    }

    /// どこかの名前空間で束縛される名前 (重複を含む)。
    pub(super) fn bound_names(&self) -> impl Iterator<Item = &str> {
        self.module_scope
            .names()
            .chain(self.classes.iter().flat_map(|class| class.scope.names()))
    }

    /// `from このモジュール import *` で入る、module 直下の名前。`__all__` が静的ならその名前
    /// (静的に束縛していない名前も、module の `__getattr__` が返しうる)、無ければ `_` で
    /// 始まらない名前、組み立てが動的なら全部 (分からないので広く取る)。重複を含む。
    pub(super) fn star_exported_names(&self) -> impl Iterator<Item = &str> {
        self.module_scope
            .names()
            .filter(|name| self.export_policy.name_is_exported(name) != Some(false))
            .chain(self.export_policy.static_names())
    }

    /// `name` を `from このモジュール import *` で他のモジュールへ入れうるか。
    pub(super) fn star_exports(&self, name: &str) -> bool {
        self.export_policy.name_is_exported(name) != Some(false)
    }

    /// `from このモジュール import *` が、静的に束縛していない `name` を module の `__getattr__` で
    /// 作りうるか (`import *` は `__all__` の名前を `getattr` で引く。`__all__` が無ければ
    /// 名前空間の名前だけを入れる)。
    pub(super) fn star_may_call_getattr(&self, name: &str) -> bool {
        self.module_getattr && self.export_policy.defines_all() && self.star_exports(name)
    }

    /// `from このモジュール import *` で入る名前を確定できない (`__all__` の組み立てが動的で、
    /// module の `__getattr__` が静的に無い名前を作りうる)。
    pub(super) fn star_names_are_dynamic(&self) -> bool {
        self.module_getattr && self.export_policy.all_is_indeterminate()
    }

    /// 開始位置 `start` の定義の qualname (メソッドは `クラス.名前`、クラスは名前だけ。シンボルの
    /// 抽出と同じく、一番内側のクラスを container にする)。
    #[cfg(test)]
    pub(super) fn definition_name(&self, start: (usize, usize)) -> Option<String> {
        self.classes.iter().find_map(|class| {
            if class.start == start {
                return Some(class.name.clone());
            }
            class
                .members
                .iter()
                .find(|(_, member)| *member == start)
                .map(|(name, _)| format!("{}.{name}", class.name))
        })
    }

    /// クラス `class` の名前空間に、テストになりうる名前の束縛 (`test*` のメソッド・別名・
    /// import、`Test*` の入れ子のクラス、テストメソッドが無いときに unittest が実行する
    /// `runTest`) があるか。
    pub(super) fn binds_test_names(&self, class: usize) -> bool {
        self.classes[class]
            .scope
            .names()
            .any(|name| name.starts_with("test") || name.starts_with("Test") || name == "runTest")
    }

    /// pytest がクラスを飛ばす (`__init__` / `__new__` が `object` のものと違う) と構文から言えるか。
    pub(super) fn skipped_by_constructor(&self, index: usize) -> bool {
        let class = &self.classes[index];
        let object_only = class.bases.is_empty()
            || (!self.object_rebound
                && class
                    .bases
                    .iter()
                    .all(|base| matches!(base, Base::Name(name) if name == "object")));
        !self.constructor_rewritten
            && !class.decorated
            && !class.keyword_arguments
            && object_only
            && (class.init == OwnBinding::Def || class.new == OwnBinding::Def)
    }
}

/// `node` の名前付きの子を、走査のスタックに積む。集合を作るだけなので訪れる順は問わない。
fn push_children<'t>(stack: &mut Vec<Visit<'t>>, node: Node<'t>, in_body: bool, decorated: bool) {
    let mut cursor = node.walk();
    stack.extend(node.named_children(&mut cursor).map(|child| Visit {
        node: child,
        in_body,
        decorated,
    }));
}

/// 比較の演算子が `is` / `is not` / `in` / `not in` だけか (どれも多重定義できず、bool を返す)。
fn only_boolean_comparisons(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    let mut operators = node
        .children_by_field_name("operators", &mut cursor)
        .peekable();
    operators.peek().is_some()
        && operators.all(|operator| matches!(operator.kind(), "is" | "is not" | "in" | "not in"))
}

/// 式の子を、先頭から処理する順で明示スタックに積む。
fn enter_children<'t>(frames: &mut Vec<Frame<'t>>, children: &[Node<'t>], depth: usize) {
    frames.extend(
        children
            .iter()
            .rev()
            .map(|child| Frame::Enter(*child, depth)),
    );
}

/// `node` のコメント以外の名前付きの子。
fn expression_children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| child.kind() != "comment")
        .collect()
}

/// `a.b.c` の属性を要素の並びにする。途中に識別子と属性以外があれば `None`。
fn dotted_path(node: Node<'_>, source: &[u8]) -> Option<Vec<String>> {
    let mut parts = Vec::new();
    let mut node = node;
    loop {
        match node.kind() {
            "identifier" => {
                parts.push(node_text(node, source).to_string());
                break;
            }
            "attribute" => {
                parts.push(node_text(node.child_by_field_name("attribute")?, source).to_string());
                node = node.child_by_field_name("object")?;
            }
            _ => return None,
        }
    }
    parts.reverse();
    Some(parts)
}

/// import の 1 つの名前 (`dotted_name` か `aliased_import`) の (経路, 別名)。
fn imported_name(node: Node<'_>, source: &[u8]) -> (Option<Vec<String>>, Option<String>) {
    match node.kind() {
        "aliased_import" => (
            node.child_by_field_name("name")
                .and_then(|path| dotted_names(path, source)),
            node.child_by_field_name("alias")
                .map(|alias| node_text(alias, source).to_string()),
        ),
        _ => (dotted_names(node, source), None),
    }
}

/// `dotted_name` (か識別子) の要素。
fn dotted_names(node: Node<'_>, source: &[u8]) -> Option<Vec<String>> {
    match node.kind() {
        "identifier" => Some(vec![node_text(node, source).to_string()]),
        "dotted_name" => {
            let parts: Vec<String> = expression_children(node)
                .into_iter()
                .filter(|child| child.kind() == "identifier")
                .map(|child| node_text(child, source).to_string())
                .collect();
            (!parts.is_empty()).then_some(parts)
        }
        _ => None,
    }
}

/// `from` の後ろのモジュール (`dotted_name` か `relative_import`)。
fn module_ref(node: Node<'_>, source: &[u8]) -> Option<ModuleRef> {
    match node.kind() {
        "dotted_name" => Some(ModuleRef {
            level: 0,
            path: dotted_names(node, source)?,
        }),
        "relative_import" => {
            let mut level = 0;
            let mut path = Vec::new();
            for child in expression_children(node) {
                match child.kind() {
                    "import_prefix" => {
                        level = node_text(child, source)
                            .bytes()
                            .filter(|byte| *byte == b'.')
                            .count();
                    }
                    "dotted_name" => path = dotted_names(child, source)?,
                    _ => {}
                }
            }
            (level > 0).then_some(ModuleRef { level, path })
        }
        _ => None,
    }
}

fn is_constructor_name(name: &str) -> bool {
    matches!(name, "__init__" | "__new__")
}

/// `classmethod(X)` の `X` の書き方 (`GenericAlias` か `types.GenericAlias`)。それ以外の形なら
/// `None` (独自の `__class_getitem__`)。束縛は走査の後で確かめる。
fn standard_getitem_argument<'a>(value: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    if value.kind() != "call"
        || value
            .child_by_field_name("function")
            .is_none_or(|function| node_text(function, source) != "classmethod")
    {
        return None;
    }
    let arguments = value.child_by_field_name("arguments")?;
    match expression_children(arguments).as_slice() {
        [argument] => {
            let text = node_text(*argument, source);
            matches!(text, "GenericAlias" | "types.GenericAlias").then_some(text)
        }
        _ => None,
    }
}

fn definition_name<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    node.child_by_field_name("name")
        .map(|name| node_text(name, source))
}

fn node_text<'a>(node: Node<'_>, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or_default()
}

fn start_of(node: Node<'_>) -> (usize, usize) {
    let start = node.start_position();
    (start.row, start.column)
}
