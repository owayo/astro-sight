//! Python のデコレータによるフレームワーク登録の判定。
//!
//! フレームワークがデコレータで登録して呼ぶ関数・メソッド・クラス (`@app.command()` /
//! `@shared_task` / Pydantic の `@model_validator` など) は、呼び出し側に名前が現れない。
//! 参照を数えるだけでは dead と誤報するので、dead-code の候補から外す。
//!
//! 判定は名前による推測で、登録を証明するものではない。dead-code では生きているものを
//! dead と言う誤りが最も悪いので、迷う形は登録とみなす側に倒す。ただし Pydantic v1 の
//! `@validator` のように名前が汎用的なものは、import の出所を確かめた場合だけ扱う。

use std::cell::OnceCell;
use std::collections::HashMap;
use std::ops::ControlFlow;

use tree_sitter::Node;

use crate::engine::tree_walk::{Children, Walk, walk_preorder};
use crate::models::location::Range;

use super::node_for_symbol_range;

/// 先頭式そのもの (別名を戻した形を含む) と一致すれば登録とみなすデコレータ。
const BARE_DECORATORS: &[&str] = &[
    "receiver",
    "login_required",
    "permission_required",
    "csrf_exempt",
    "cache_page",
    "require_GET",
    "require_POST",
    "require_http_methods",
    "require_safe",
    "staff_member_required",
    "user_passes_test",
];

/// Django のテンプレートタグ・フィルタ (`register = template.Library()` への登録。
/// テンプレートからしか呼ばれない) と admin の登録 (`@admin.register(Model)` の ModelAdmin)。
/// 末尾名 `register` / `filter` / `tag` を広く足すと無関係なデコレータまで拾うので、
/// Django が名前を決めている `register.<種類>` と `admin.register` の形だけに絞る。
const QUALIFIED_DECORATORS: &[&str] = &[
    "register.filter",
    "register.simple_tag",
    "register.inclusion_tag",
    "register.tag",
    "admin.register",
];

/// 末尾の名前で照合するデコレータ。
const TAIL_DECORATORS: &[&str] = &[
    // Typer / Click / Discord.py / aiogram
    "command",
    "callback",
    "group",
    "event",
    // FastAPI / Flask HTTP methods
    "get",
    "post",
    "put",
    "delete",
    "patch",
    "options",
    "head",
    "route",
    "websocket",
    // FastAPI / Flask lifecycle
    "middleware",
    "on_event",
    "exception_handler",
    "before_request",
    "after_request",
    "errorhandler",
    "teardown_request",
    "teardown_appcontext",
    "context_processor",
    "shell_context_processor",
    "before_first_request",
    // Celery / RQ (`@app.task` と、アプリを持たない再利用タスクの `@shared_task`)
    "task",
    "shared_task",
    // pytest
    "fixture",
    "parametrize",
    "usefixtures",
    // 汎用の登録デコレータ (MCP `@server.tool(...)`, イベントバス `@bus.subscribe(...)` 等)。
    // 「関数を変換するだけ」のデコレータ (`lru_cache` / `wraps` / `dataclass` / retry /
    // `validate_call` のような引数検証) は真の dead を隠すため入れない。登録先へハンドラを
    // 預ける意味が名前から明確なものだけに絞る。
    "handler",
    "tool",
    "listener",
    "subscriber",
    "subscribe",
    "consumer",
];

/// Pydantic がモデルの検証・シリアライズのときに呼ぶメソッドを登録するデコレータ
/// (v2 の validator / serializer / computed field と v1 の `root_validator`)。
///
/// 名前が Pydantic に固有なので、出所を確かめずに末尾の名前で照合する。出所の証明を
/// 求めると、`from pydantic import *` や自前のモジュールからの再エクスポートで使ったものを
/// dead と誤報する。クラス本体で定義した関数にだけ使う。
const PYDANTIC_MEMBER_DECORATORS: &[&str] = &[
    "model_validator",
    "field_validator",
    "field_serializer",
    "model_serializer",
    "computed_field",
    "root_validator",
];

/// Pydantic v1 の `@validator` の完全名。名前が汎用的なので (自作のデコレータや attrs の
/// `@x.validator` もある)、pydantic からの import で束縛した名前だと確かめた場合だけ
/// 登録として扱う。クラス本体で定義した関数にだけ使う。
const PYDANTIC_V1_VALIDATOR_PATHS: &[&str] = &[
    "pydantic.validator",
    "pydantic.v1.validator",
    "pydantic.class_validators.validator",
    "pydantic.v1.class_validators.validator",
];

/// デコレータの評価に効く import の束縛を、スコープごとに 1 ファイル分まとめた索引。
///
/// `from celery import shared_task as st` の `@st` や `from pydantic import model_validator as mv`
/// の `@mv(...)` を、元の名前で照合するために使う。dead-code の 1 ファイルの判定の中で
/// 1 回だけ作る (`has_framework_entrypoint_decorator_python` に `OnceCell` で渡す)。
///
/// 束縛はスコープ単位で持つ。ファイル全体を 1 つの表にまとめると、無関係な関数の中の
/// import がモジュール直下のデコレータに効いてしまう。デコレータを付けた関数自身の本体の
/// import も、そのデコレータには効かない (外側のスコープで評価されるため)。
/// 同じスコープの中の位置 (import より前で使っている、後で束縛し直している) は見ない。
/// どれも登録とみなす側にしか倒れない。
#[derive(Debug)]
pub struct PythonDecoratorImports {
    /// 添字 0 がモジュール。関数・クラスの本体ごとに 1 つ。
    scopes: Vec<ImportScope>,
    /// `function_definition` / `class_definition` の node id → それを直接含むスコープの添字。
    definition_scope: HashMap<usize, usize>,
}

#[derive(Debug, Default)]
struct ImportScope {
    parent: Option<usize>,
    /// クラスの本体か。Pydantic のデコレータはクラス本体の関数にだけ使う。
    is_class_body: bool,
    /// import が束縛した名前 → 出所 (if / try の分岐ごとに複数ありうる)。
    bindings: HashMap<String, Vec<ImportOrigin>>,
    /// `from M import *` の `M`。
    star_modules: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct ImportOrigin {
    /// import で書いた元の名前。`from celery import shared_task as st` なら `shared_task`、
    /// `import a.b as x` なら `a.b`。
    imported: String,
    /// 出所を付けた完全名。前者は `celery.shared_task`、後者は `a.b`。
    qualified: String,
}

impl PythonDecoratorImports {
    /// 文を含みうるノードだけを辿り、import 文とスコープを集める。
    pub fn build(root: Node, source: &[u8]) -> Self {
        let mut scopes = vec![ImportScope::default()];
        let mut definition_scope = HashMap::new();
        // (スコープを作ったノードの深さ, スコープの添字)。モジュールは取り除かない。
        let mut stack: Vec<(usize, usize)> = vec![(0, 0)];
        let _: ControlFlow<()> = walk_preorder(root, Children::Named, |node, depth| {
            while stack.len() > 1 && stack.last().is_some_and(|&(d, _)| d >= depth) {
                stack.pop();
            }
            let current = stack.last().map_or(0, |&(_, scope)| scope);
            let walk = match node.kind() {
                kind @ ("function_definition" | "class_definition") => {
                    definition_scope.insert(node.id(), current);
                    scopes.push(ImportScope {
                        parent: Some(current),
                        is_class_body: kind == "class_definition",
                        ..ImportScope::default()
                    });
                    stack.push((depth, scopes.len() - 1));
                    Walk::Descend
                }
                "import_statement" | "import_from_statement" => {
                    collect_import(node, source, &mut scopes[current]);
                    Walk::Skip
                }
                kind if may_contain_statements(kind) => Walk::Descend,
                _ => Walk::Skip,
            };
            ControlFlow::Continue(walk)
        });
        Self {
            scopes,
            definition_scope,
        }
    }

    /// `definition` を含むスコープから外側へ向かって並べる。
    fn enclosing_scopes(&self, definition: Node) -> impl Iterator<Item = &ImportScope> {
        let mut next = Some(
            self.definition_scope
                .get(&definition.id())
                .copied()
                .unwrap_or(0),
        );
        std::iter::from_fn(move || {
            let scope = &self.scopes[next?];
            next = scope.parent;
            Some(scope)
        })
    }

    /// `definition` がクラスの本体で定義されているか (本体の中の if / try の中も含む)。
    fn defined_in_class_body(&self, definition: Node) -> bool {
        self.enclosing_scopes(definition)
            .next()
            .is_some_and(|scope| scope.is_class_body)
    }
}

/// Python の文を子孫に持ちうるノードか。文はモジュールとブロックの直下にだけ現れ、
/// ブロックを持つのは複合文 (`if_statement` 等)・節 (`elif_clause` / `except_clause` /
/// `case_clause` 等)・定義 (`decorated_definition` 等) なので、種類名の接尾辞で決める
/// (式の中には文が無いので降りない)。解析エラーの中は念のため辿る。
fn may_contain_statements(kind: &str) -> bool {
    matches!(kind, "module" | "block" | "ERROR")
        || kind.ends_with("_statement")
        || kind.ends_with("_clause")
        || kind.ends_with("_definition")
}

fn collect_import(statement: Node, source: &[u8], scope: &mut ImportScope) {
    let text = |node: Node| dotted_name_text(node, source);
    let module = statement.child_by_field_name("module_name").and_then(text);
    let mut cursor = statement.walk();
    if statement
        .named_children(&mut cursor)
        .any(|child| child.kind() == "wildcard_import")
    {
        if let Some(module) = module
            && !scope.star_modules.contains(&module)
        {
            scope.star_modules.push(module);
        }
        return;
    }
    let mut cursor = statement.walk();
    for child in statement.children_by_field_name("name", &mut cursor) {
        let (name, alias) = match child.kind() {
            "aliased_import" => (
                child.child_by_field_name("name").and_then(text),
                child.child_by_field_name("alias").and_then(text),
            ),
            _ => (text(child), None),
        };
        let Some(name) = name else {
            continue;
        };
        let (local, origin) = match &module {
            // `from M import N [as A]`: A (なければ N) を束縛する。
            Some(module) => {
                let qualified = if module.ends_with('.') {
                    format!("{module}{name}")
                } else {
                    format!("{module}.{name}")
                };
                (
                    alias.unwrap_or_else(|| name.clone()),
                    ImportOrigin {
                        imported: name,
                        qualified,
                    },
                )
            }
            // `import a.b as x` は x を a.b に、`import a.b` は先頭の a を a に束縛する。
            None => match alias {
                Some(alias) => (
                    alias,
                    ImportOrigin {
                        imported: name.clone(),
                        qualified: name,
                    },
                ),
                None => {
                    let head = name.split('.').next().unwrap_or(&name).to_string();
                    (
                        head.clone(),
                        ImportOrigin {
                            imported: head.clone(),
                            qualified: head,
                        },
                    )
                }
            },
        };
        let origins = scope.bindings.entry(local).or_default();
        if !origins.contains(&origin) {
            origins.push(origin);
        }
    }
}

/// import の名前 (`identifier` / `dotted_name` / `relative_import`) を、識別子とドットだけで
/// 組み立てる。テキストをそのまま使うと、名前の途中の行継続 (`from pydantic.\` + 改行 +
/// `v1 import validator`) や空白まで名前に入る。
fn dotted_name_text(node: Node, source: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" => node.utf8_text(source).ok().map(str::to_string),
        "dotted_name" => {
            let mut cursor = node.walk();
            let parts = node
                .named_children(&mut cursor)
                .filter(|child| child.kind() == "identifier")
                .map(|child| child.utf8_text(source).ok())
                .collect::<Option<Vec<_>>>()?;
            (!parts.is_empty()).then(|| parts.join("."))
        }
        // `from . import x` / `from ..pkg import y`: 先頭のドットを数えて残す。
        "relative_import" => {
            let mut text = String::new();
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                match child.kind() {
                    "import_prefix" => {
                        text.extend(child.utf8_text(source).ok()?.chars().filter(|&c| c == '.'))
                    }
                    "dotted_name" => text.push_str(&dotted_name_text(child, source)?),
                    _ => {}
                }
            }
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

/// デコレータの先頭式 (`@app.route("/")` の `app.route`)。
struct DecoratorHead {
    /// 識別子と属性名の並び。
    segments: Vec<String>,
    /// 先頭まで名前の連鎖か。`@factory().task` / `@routes["a"].get` のように受け手が名前で
    /// ないときは偽で、`segments` は受け手より後ろの属性名だけを持つ。
    complete: bool,
}

/// デコレータの先頭式を構文木から取る。
///
/// テキストを `(` で切ると、行末コメント (`@shared_task  # celery`。コメントも decorator
/// ノードの子になる) や括弧・空白を挟んだ書き方で名前が崩れる。呼び出し (`@a.b(...)` /
/// `@a.b(...)(...)`) と括弧を外し、識別子と属性の連鎖だけを名前にする。
///
/// 受け手が呼び出しのとき (`@shared_task().__call__` / `@get_app().task`) は、末尾の属性名に
/// 加えて、受け手の呼び出し先の名前も先頭式として返す。テキストを `(` で切っていた旧実装は
/// 後者を名前にしていたので、どちらで照合しても旧実装より登録が減らない。
fn decorator_heads(decorator: Node, source: &[u8]) -> Vec<DecoratorHead> {
    let mut heads = Vec::new();
    let mut next = first_non_comment_named_child(decorator);
    while let Some(mut expression) = next.take() {
        // 呼び出しと括弧を外す。
        let unwrapped = loop {
            expression = match expression.kind() {
                "call" => match expression.child_by_field_name("function") {
                    Some(function) => function,
                    None => break false,
                },
                "parenthesized_expression" => match first_non_comment_named_child(expression) {
                    Some(inner) => inner,
                    None => break false,
                },
                _ => break true,
            };
        };
        if !unwrapped {
            break;
        }
        let mut segments = Vec::new();
        let mut node = expression;
        let complete = loop {
            match node.kind() {
                "identifier" => {
                    if let Ok(text) = node.utf8_text(source) {
                        segments.push(text.to_string());
                    }
                    break true;
                }
                "attribute" => {
                    let (Some(name), Some(object)) = (
                        node.child_by_field_name("attribute"),
                        node.child_by_field_name("object"),
                    ) else {
                        break false;
                    };
                    let Ok(text) = name.utf8_text(source) else {
                        break false;
                    };
                    segments.push(text.to_string());
                    node = object;
                }
                "parenthesized_expression" => match first_non_comment_named_child(node) {
                    Some(inner) => node = inner,
                    None => break false,
                },
                "call" => {
                    next = Some(node);
                    break false;
                }
                _ => break false,
            }
        };
        if !segments.is_empty() {
            segments.reverse();
            heads.push(DecoratorHead { segments, complete });
        }
    }
    heads
}

fn first_non_comment_named_child(node: Node) -> Option<Node> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| child.kind() != "comment")
}

/// 照合に使う名前。
#[derive(Default)]
struct HeadNames {
    /// 書いたままの名前と、先頭の別名を import の元の名前に戻した名前 (`shared_task` /
    /// `admin.register`)。完全な連鎖のときだけ入る。
    local: Vec<String>,
    /// import の出所を付けた完全名 (`celery.shared_task` / `pydantic.v1.validator`)。
    /// import で束縛を確かめた名前だけが入る。
    imported: Vec<String>,
    /// 末尾の名前。
    tails: Vec<String>,
}

impl HeadNames {
    fn written(head: &DecoratorHead) -> Self {
        let mut names = Self::default();
        if let Some(last) = head.segments.last() {
            names.tails.push(last.clone());
        }
        if head.complete {
            names.local.push(head.segments.join("."));
        }
        names
    }

    /// 書いたままの名前に、先頭の名前を import の束縛で戻した名前を足す。
    fn resolved(head: &DecoratorHead, scopes: &[&ImportScope]) -> Self {
        let mut names = Self::written(head);
        if !head.complete {
            return names;
        }
        let written = head.segments.join(".");
        let first = &head.segments[0];
        let rest = &head.segments[1..];
        let join = |base: &str| {
            std::iter::once(base)
                .chain(rest.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(".")
        };
        for scope in scopes {
            for origin in scope.bindings.get(first).into_iter().flatten() {
                let local = join(&origin.imported);
                if let Some(tail) = local.rsplit('.').next() {
                    names.tails.push(tail.to_string());
                }
                names.local.push(local);
                names.imported.push(join(&origin.qualified));
            }
            for module in &scope.star_modules {
                names.imported.push(format!("{module}.{written}"));
            }
        }
        names
    }

    /// 定義の種類を問わない登録 (Django / Celery / FastAPI / pytest 等) か。
    ///
    /// 書いた名前と別名を戻した名前は完全一致で照合する。import の出所を付けた完全名は
    /// 末尾の一致で照合する (`import django.contrib.admin as dj` の `@dj.register` は
    /// `django.contrib.admin.register` になり、`admin.register` で終わる)。
    fn matches_general(&self) -> bool {
        let named = |name: &str, pattern: &str| {
            name.strip_suffix(pattern)
                .is_some_and(|prefix| prefix.is_empty() || prefix.ends_with('.'))
        };
        self.local
            .iter()
            .chain(&self.imported)
            .any(|name| name.starts_with("pytest.mark."))
            || self.local.iter().any(|name| {
                BARE_DECORATORS.contains(&name.as_str())
                    || QUALIFIED_DECORATORS.contains(&name.as_str())
            })
            || self.imported.iter().any(|name| {
                BARE_DECORATORS
                    .iter()
                    .chain(QUALIFIED_DECORATORS)
                    .any(|pattern| named(name, pattern))
            })
            || self
                .tails
                .iter()
                .any(|tail| TAIL_DECORATORS.contains(&tail.as_str()))
    }

    /// クラス本体の関数に付く Pydantic の登録か。
    fn matches_pydantic_member(&self) -> bool {
        self.tails
            .iter()
            .any(|tail| PYDANTIC_MEMBER_DECORATORS.contains(&tail.as_str()))
            || self
                .imported
                .iter()
                .any(|name| PYDANTIC_V1_VALIDATOR_PATHS.contains(&name.as_str()))
    }
}

/// フレームワークの登録デコレータで装飾された関数・メソッド・クラスか。
///
/// `imports` は同じファイルの判定で共有し、名前を書いたままでは決まらないときに 1 回だけ作る。
pub fn has_framework_entrypoint_decorator_python(
    root: Node,
    source: &[u8],
    symbol_range: &Range,
    imports: &OnceCell<PythonDecoratorImports>,
) -> bool {
    let Some(node) = node_for_symbol_range(root, symbol_range) else {
        return false;
    };

    // 関数 / メソッド / クラス定義の祖先を探す
    let mut current = Some(node);
    let mut definition = None;
    while let Some(n) = current {
        if matches!(n.kind(), "function_definition" | "class_definition") {
            definition = Some(n);
            break;
        }
        current = n.parent();
    }
    let Some(definition) = definition else {
        return false;
    };

    // 親が decorated_definition でなければデコレータ無し
    let Some(parent) = definition.parent() else {
        return false;
    };
    if parent.kind() != "decorated_definition" {
        return false;
    }

    let mut cursor = parent.walk();
    let heads: Vec<DecoratorHead> = parent
        .children(&mut cursor)
        .filter(|child| child.kind() == "decorator")
        .flat_map(|decorator| decorator_heads(decorator, source))
        .collect();
    if heads.is_empty() {
        return false;
    }
    if heads
        .iter()
        .any(|head| HeadNames::written(head).matches_general())
    {
        return true;
    }

    let imports = imports.get_or_init(|| PythonDecoratorImports::build(root, source));
    let scopes: Vec<&ImportScope> = imports.enclosing_scopes(definition).collect();
    let class_member =
        definition.kind() == "function_definition" && imports.defined_in_class_body(definition);
    heads.iter().any(|head| {
        let names = HeadNames::resolved(head, &scopes);
        names.matches_general() || (class_member && names.matches_pydantic_member())
    })
}
