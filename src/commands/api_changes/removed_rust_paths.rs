//! 削除判定専用: Rust のパス付き参照と素の参照が、削除したモジュール自由関数とは別の項目を
//! 指すことの証拠 (Issue #110)。
//!
//! 辿るのは明示的に宣言された `mod`・型・自由関数だけ。同じモジュールの明示的な item は
//! グロブ import より優先され、明示 import や macro 展開の item と同名なら compile error に
//! なるので、モジュール直下のマクロやグロブがあっても辿った先は変わらない。参照から
//! モジュールまでの間で名前を束縛し得るもの、トークンを書き換え得る属性とマクロの中は
//! 確かめられないので分類しない (呼び出し側で未証明のまま残る)。ファイルをまたぐ解決は、
//! 全所属の証拠が一致し、未知の所属が無いと確かめられたときだけ行う。

use std::cell::{OnceCell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;

use tree_sitter::Node;

#[cfg(test)]
use crate::{engine::parser, language::LangId};

#[path = "removed_rust_layout.rs"]
mod layout;

#[path = "removed_rust_dependencies.rs"]
mod dependencies;

#[cfg(test)]
#[path = "removed_rust_layout_tests.rs"]
mod layout_tests;

#[cfg(test)]
#[path = "removed_rust_dependency_tests.rs"]
mod dependency_tests;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PathSite {
    /// 型を修飾部に持つ関連項目のパス (`Type::name`・`Self::name`・`<T>::name`)。
    Associated,
    /// 残るモジュールが直接宣言する自由関数。値は宣言したファイル (repo 相対、`/` 区切り)。
    ModuleFunction(Rc<str>),
}

/// 属性の扱い。`derive` は import で別の属性マクロに置き換えられるので、ファイルに `derive` を
/// 持ち込み得る use・extern crate があれば未知の属性として扱う。
#[derive(Clone, Copy)]
struct AttributePolicy {
    derive_shadowed: bool,
}

impl AttributePolicy {
    fn for_tree(root: Node<'_>, source: &[u8]) -> Self {
        let mut derive_shadowed = false;
        let _: std::ops::ControlFlow<()> = crate::engine::tree_walk::walk_preorder(
            root,
            crate::engine::tree_walk::Children::Named,
            |node, _| {
                if matches!(node.kind(), "use_declaration" | "extern_crate_declaration") {
                    if node.kind() == "extern_crate_declaration"
                        || identifier_tokens(node, source)
                            .iter()
                            .any(|token| token == "derive")
                    {
                        derive_shadowed = true;
                        return std::ops::ControlFlow::Break(());
                    }
                    return std::ops::ControlFlow::Continue(crate::engine::tree_walk::Walk::Skip);
                }
                std::ops::ControlFlow::Continue(crate::engine::tree_walk::Walk::Descend)
            },
        );
        Self { derive_shadowed }
    }

    /// item の有無を変えず、トークンも書き換えない組み込み属性。
    fn keeps_item(self, name: &str) -> bool {
        matches!(
            name,
            "allow"
                | "warn"
                | "deny"
                | "forbid"
                | "expect"
                | "doc"
                | "deprecated"
                | "must_use"
                | "inline"
                | "cold"
                | "track_caller"
                | "repr"
                | "non_exhaustive"
        )
    }

    /// 型の有無も変えない属性。derive は元の item を変えずに item を足すだけ。
    fn keeps_type(self, name: &str) -> bool {
        self.keeps_item(name) || (name == "derive" && !self.derive_shadowed)
    }

    /// トークンを書き換えない属性。`cfg` は item を条件付きにするだけ。
    fn keeps_tokens(self, name: &str) -> bool {
        self.keeps_type(name) || name == "cfg"
    }

    /// モジュール全体に付く inner attribute で、名前の解決を変えないもの。
    fn keeps_scope(self, name: &str) -> bool {
        self.keeps_tokens(name)
            || matches!(
                name,
                "no_std"
                    | "no_implicit_prelude"
                    | "feature"
                    | "recursion_limit"
                    | "type_length_limit"
                    | "crate_name"
                    | "crate_type"
                    | "windows_subsystem"
            )
    }
}

/// 属性の名前の並び。`cfg_attr` は条件の後に並ぶ属性の名前を返す。パス付きの属性
/// (`#[tokio::main]`) や読めない形は None。
fn attribute_names<'s>(item: Node<'_>, source: &'s [u8]) -> Option<Vec<&'s str>> {
    let mut cursor = item.walk();
    let attribute = item
        .named_children(&mut cursor)
        .find(|child| child.kind() == "attribute")?;
    let name = attribute.named_child(0)?;
    if name.kind() != "identifier" {
        return None;
    }
    let name = name.utf8_text(source).ok()?;
    if name != "cfg_attr" {
        return Some(vec![name]);
    }
    let arguments = attribute.child_by_field_name("arguments")?;
    let mut names = Vec::new();
    // 最初の組は条件。カンマで区切った後の組ごとに、先頭の識別子が属性の名前。
    let mut group = 0usize;
    let mut group_start = true;
    let mut pending: Option<&str> = None;
    let mut cursor = arguments.walk();
    for token in arguments.children(&mut cursor) {
        match token.kind() {
            "(" | ")" | "line_comment" | "block_comment" => {}
            "," => {
                names.extend(pending.take());
                group += 1;
                group_start = true;
            }
            _ if group == 0 => {}
            kind => {
                if group_start {
                    if kind != "identifier" {
                        return None;
                    }
                    pending = Some(token.utf8_text(source).ok()?);
                    group_start = false;
                } else if pending.is_some() && kind == "::" {
                    return None;
                } else {
                    names.extend(pending.take());
                }
            }
        }
    }
    names.extend(pending);
    (group > 0).then_some(names)
}

fn attribute_allowed(attribute: Node<'_>, source: &[u8], allowed: impl Fn(&str) -> bool) -> bool {
    attribute_names(attribute, source).is_some_and(|names| names.into_iter().all(allowed))
}

#[derive(Clone, Copy)]
struct AttributeAllowance {
    item: bool,
    typ: bool,
    tokens: bool,
}

impl Default for AttributeAllowance {
    fn default() -> Self {
        Self {
            item: true,
            typ: true,
            tokens: true,
        }
    }
}

/// 属性・コメントの連続を兄弟ごとに一度だけ要約する。判定規則を混ぜない。
#[derive(Default)]
struct AttributeSummaries {
    outer: HashMap<usize, AttributeAllowance>,
    inner: HashMap<usize, bool>,
}

impl AttributeSummaries {
    fn collect(root: Node<'_>, source: &[u8], policy: AttributePolicy) -> Self {
        let mut result = Self::default();
        let mut pending = vec![root];
        while let Some(parent) = pending.pop() {
            let mut allowance = AttributeAllowance::default();
            let mut inner = true;
            let mut cursor = parent.walk();
            for child in parent.named_children(&mut cursor) {
                pending.push(child);
                match child.kind() {
                    "attribute_item" => {
                        let names = attribute_names(child, source);
                        allowance.item &= names
                            .as_ref()
                            .is_some_and(|names| names.iter().all(|name| policy.keeps_item(name)));
                        allowance.typ &= names
                            .as_ref()
                            .is_some_and(|names| names.iter().all(|name| policy.keeps_type(name)));
                        allowance.tokens &= names.as_ref().is_some_and(|names| {
                            names.iter().all(|name| policy.keeps_tokens(name))
                        });
                    }
                    "line_comment" | "block_comment" => {}
                    _ => {
                        result.outer.insert(child.id(), allowance);
                        allowance = AttributeAllowance::default();
                    }
                }
                if child.kind() == "inner_attribute_item" {
                    inner &= attribute_allowed(child, source, |name| policy.keeps_scope(name));
                }
            }
            result.inner.insert(parent.id(), inner);
        }
        result
    }

    fn outer(&self, node: Node<'_>) -> AttributeAllowance {
        self.outer.get(&node.id()).copied().unwrap_or_default()
    }
}

fn inner_attributes_satisfy(scope: Node<'_>, source: &[u8], policy: AttributePolicy) -> bool {
    let mut cursor = scope.walk();
    scope
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "inner_attribute_item")
        .all(|attribute| attribute_allowed(attribute, source, |name| policy.keeps_scope(name)))
}

/// ノードそのものが、トークンを書き換え得る属性やマクロの token tree、use 宣言、可視性で
/// なく、スコープなら inner attribute が、item なら直前の属性が名前の解決を変えないか。
fn locally_rewrite_free(node: Node<'_>, attributes: &AttributeSummaries) -> bool {
    !matches!(
        node.kind(),
        "token_tree"
            | "attribute_item"
            | "inner_attribute_item"
            | "macro_definition"
            | "use_declaration"
            | "visibility_modifier"
    ) && (!matches!(node.kind(), "source_file" | "declaration_list" | "block")
        || attributes.inner.get(&node.id()) == Some(&true))
        && attributes.outer(node).tokens
}

/// 組み込み derive の名前を置き換え得る import は、ファイル全体から一度だけ集める。
fn file_import_tokens(root: Node<'_>, source: &[u8]) -> HashSet<String> {
    let mut tokens = HashSet::new();
    let _: std::ops::ControlFlow<()> = crate::engine::tree_walk::walk_preorder(
        root,
        crate::engine::tree_walk::Children::Named,
        |node, _| {
            if matches!(node.kind(), "use_declaration" | "extern_crate_declaration") {
                tokens.extend(identifier_tokens(node, source));
                if node.utf8_text(source).is_ok_and(|text| text.contains('*')) {
                    tokens.insert("*".to_owned());
                }
                return std::ops::ControlFlow::Continue(crate::engine::tree_walk::Walk::Skip);
            }
            std::ops::ControlFlow::Continue(crate::engine::tree_walk::Walk::Descend)
        },
    );
    tokens
}

/// 外部クレートの名前は、同名の直接宣言がないときにも展開が隠し得る。
fn attribute_may_generate(
    item: Node<'_>,
    source: &[u8],
    policy: AttributePolicy,
    imported: &HashSet<String>,
) -> bool {
    let Some(names) = attribute_names(item, source) else {
        return true;
    };
    let direct_derive = item
        .named_child(0)
        .and_then(|attribute| attribute.named_child(0))
        .and_then(|name| name.utf8_text(source).ok())
        == Some("derive");
    names.iter().any(|name| {
        if policy.keeps_item(name) || *name == "cfg" {
            return false;
        }
        *name != "derive"
            || !direct_derive
            || policy.derive_shadowed
            || !builtin_derives(item, source, imported)
    })
}

fn builtin_derives(item: Node<'_>, source: &[u8], imported: &HashSet<String>) -> bool {
    if imported.contains("*") {
        return false;
    }
    let Some(arguments) = item
        .named_child(0)
        .and_then(|attribute| attribute.child_by_field_name("arguments"))
    else {
        return false;
    };
    let mut cursor = arguments.walk();
    arguments
        .children(&mut cursor)
        .all(|token| match token.kind() {
            "(" | ")" | "," | "line_comment" | "block_comment" => true,
            "identifier" => token.utf8_text(source).is_ok_and(|name| {
                matches!(
                    name,
                    "Clone"
                        | "Copy"
                        | "Debug"
                        | "Default"
                        | "Eq"
                        | "PartialEq"
                        | "Ord"
                        | "PartialOrd"
                        | "Hash"
                ) && !imported.contains(name)
            }),
            _ => false,
        })
}

fn plain_name(name: &str) -> &str {
    name.strip_prefix("r#").unwrap_or(name)
}

fn declared_name<'s>(item: Node<'_>, source: &'s [u8]) -> Option<&'s str> {
    item.child_by_field_name("name")
        .and_then(|name| name.utf8_text(source).ok())
        .map(plain_name)
}

/// use 宣言や extern crate に現れる識別子。
fn identifier_tokens(node: Node<'_>, source: &[u8]) -> HashSet<String> {
    identifiers_in(node, source)
}

fn is_module_scope(node: Node<'_>) -> bool {
    node.kind() == "source_file"
        || (node.kind() == "declaration_list"
            && node
                .parent()
                .is_some_and(|owner| owner.kind() == "mod_item"))
}

#[derive(Clone, Copy)]
enum ChildModule {
    Inline(usize),
    File,
}

/// モジュールのスコープ (ファイル直下かインライン mod の本体) が直接宣言する名前。
#[derive(Default)]
struct ScopeFacts {
    /// 値の名前空間の宣言数 (属性を問わない)。
    values: HashMap<String, usize>,
    /// 有無が属性に左右されない自由関数の宣言数。
    functions: HashMap<String, usize>,
    /// 型の名前空間の宣言数 (属性を問わない。mod を含む)。
    types: HashMap<String, usize>,
    /// NFC 同値の別表記を否定できない。外部依存への fallback だけを保留する。
    non_ascii_type_names: bool,
    /// 有無が属性に左右されない型 (struct・enum・union・trait・type) の宣言数。
    plain_types: HashMap<String, usize>,
    /// 有無と配置が属性に左右されない子 mod。
    modules: HashMap<String, ChildModule>,
    /// 監査では条件付き・重複した宣言もすべて辿る。
    audit_modules: Vec<(String, ChildModule)>,
    unknown_children: bool,
    /// use 宣言と extern crate に現れる識別子。
    import_tokens: HashSet<String>,
    extern_crates: HashSet<String>,
    glob: bool,
    may_generate: bool,
    no_implicit_prelude: bool,
    /// cfg_attr 経由を含む inner cfg。依存 root の存在証明だけで使う。
    inner_cfg: bool,
    /// トークンを書き換え得る inner attribute がある。
    opaque: bool,
    parent: Option<usize>,
}

impl ScopeFacts {
    fn declares_function(&self, name: &str) -> bool {
        !self.opaque
            && self.functions.get(name) == Some(&1)
            && self.values.get(name) == Some(&1)
            && !self.import_tokens.contains(name)
    }

    fn declares_type(&self, name: &str) -> bool {
        !self.opaque
            && self.plain_types.get(name) == Some(&1)
            && self.types.get(name) == Some(&1)
            && !self.import_tokens.contains(name)
    }

    fn child_module(&self, name: &str) -> Option<ChildModule> {
        if self.opaque || self.types.get(name) != Some(&1) || self.import_tokens.contains(name) {
            return None;
        }
        self.modules.get(name).copied()
    }

    fn lacks_type_name(&self, name: &str) -> bool {
        !self.opaque
            && !self.non_ascii_type_names
            && !self.unknown_children
            && !self.may_generate
            && !self.glob
            && !self.types.contains_key(name)
            && !self.import_tokens.contains(name)
    }
}

fn count(map: &mut HashMap<String, usize>, name: &str) {
    *map.entry(name.to_owned()).or_default() += 1;
}

/// 1 ファイルのモジュールのスコープ。0 番がファイル直下。
struct FileScopes {
    scopes: Vec<ScopeFacts>,
    policy: AttributePolicy,
}

impl FileScopes {
    /// 解析エラーや欠落ノードのあるファイルは扱わない。`ids` にスコープのノード id と番号を返す。
    fn collect(
        root: Node<'_>,
        source: &[u8],
        mut ids: Option<&mut HashMap<usize, usize>>,
    ) -> Option<Self> {
        if !super::rust_ref_sites::clean_tree(root) {
            return None;
        }
        let policy = AttributePolicy::for_tree(root, source);
        let attributes = AttributeSummaries::collect(root, source, policy);
        let imported = file_import_tokens(root, source);
        let mut scopes = Vec::new();
        // 番号は積んだ順に振り、同じ順に取り出して埋める。
        let mut pending = VecDeque::from([(root, None)]);
        while let Some((scope, parent)) = pending.pop_front() {
            let index = scopes.len();
            if let Some(ids) = ids.as_deref_mut() {
                ids.insert(scope.id(), index);
            }
            let mut facts = ScopeFacts {
                opaque: !inner_attributes_satisfy(scope, source, policy),
                parent,
                ..ScopeFacts::default()
            };
            let mut cursor = scope.walk();
            for item in scope.named_children(&mut cursor) {
                let name = declared_name(item, source);
                match item.kind() {
                    "attribute_item" => {
                        facts.may_generate |=
                            attribute_may_generate(item, source, policy, &imported);
                    }
                    "inner_attribute_item" => {
                        let names = attribute_names(item, source);
                        facts.inner_cfg |=
                            names.as_ref().is_some_and(|names| names.contains(&"cfg"));
                        facts.no_implicit_prelude |=
                            names.is_none_or(|names| names.contains(&"no_implicit_prelude"));
                    }
                    "use_declaration" => {
                        facts.glob |= item.utf8_text(source).is_ok_and(|text| text.contains('*'));
                    }
                    "extern_crate_declaration" => {
                        facts.extern_crates.extend(identifier_tokens(item, source));
                    }
                    _ => {}
                }
                match (item.kind(), name) {
                    ("function_item", Some(name)) => {
                        count(&mut facts.values, name);
                        if attributes.outer(item).item {
                            count(&mut facts.functions, name);
                        }
                    }
                    ("const_item" | "static_item", Some(name)) => count(&mut facts.values, name),
                    ("struct_item", Some(name)) => {
                        count(&mut facts.types, name);
                        if attributes.outer(item).typ {
                            count(&mut facts.plain_types, name);
                        }
                        // 単位構造体と tuple 構造体は値の名前空間にも構築子を置く。
                        if item
                            .child_by_field_name("body")
                            .is_none_or(|body| body.kind() != "field_declaration_list")
                        {
                            count(&mut facts.values, name);
                        }
                    }
                    ("enum_item" | "union_item" | "trait_item" | "type_item", Some(name)) => {
                        count(&mut facts.types, name);
                        if attributes.outer(item).typ {
                            count(&mut facts.plain_types, name);
                        }
                    }
                    ("mod_item", Some(name)) => {
                        count(&mut facts.types, name);
                        let child = match item.child_by_field_name("body") {
                            Some(body) => {
                                pending.push_back((body, Some(index)));
                                ChildModule::Inline(index + pending.len())
                            }
                            None => ChildModule::File,
                        };
                        facts.audit_modules.push((name.to_owned(), child));
                        if attributes.outer(item).item {
                            facts.modules.insert(name.to_owned(), child);
                        }
                    }
                    ("foreign_mod_item", _) => {
                        let Some(body) = item.child_by_field_name("body") else {
                            continue;
                        };
                        let mut cursor = body.walk();
                        for declaration in body.named_children(&mut cursor) {
                            let Some(name) = declared_name(declaration, source) else {
                                continue;
                            };
                            if declaration.kind() == "associated_type" {
                                count(&mut facts.types, name);
                            } else {
                                count(&mut facts.values, name);
                            }
                        }
                    }
                    ("use_declaration" | "extern_crate_declaration", _) => {
                        let names = identifier_tokens(item, source);
                        // Rust は NFC で名前を比較する。正規化しない非 ASCII の束縛は保留する。
                        facts.opaque |= names.iter().any(|name| !name.is_ascii());
                        facts.import_tokens.extend(names);
                    }
                    ("macro_invocation", _) => facts.unknown_children = true,
                    ("expression_statement", _)
                        if item
                            .named_child(0)
                            .is_some_and(|child| child.kind() == "macro_invocation") =>
                    {
                        facts.unknown_children = true
                    }
                    _ => {}
                }
            }
            facts.unknown_children |= facts.opaque;
            facts.non_ascii_type_names = facts.types.keys().any(|name| !name.is_ascii());
            scopes.push(facts);
        }
        Some(Self { scopes, policy })
    }
}

#[derive(Clone)]
struct ModuleRef {
    /// repo 相対、`/` 区切り。
    file: Rc<str>,
    scope: usize,
    occurrence: Option<usize>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Namespace {
    Value,
    Type,
}

enum Lexical<'t> {
    /// 参照の属するモジュールのスコープのノード。
    Module(Node<'t>),
    /// 見えている総称型の型引数。
    GenericType,
}

enum Resolved {
    Module(ModuleRef),
    Type,
}

/// 参照ファイル 1 つ分の解析。スコープの番号はノード id から引く。
struct RefFile {
    path: Rc<str>,
    scopes: Rc<FileScopes>,
    ids: HashMap<usize, usize>,
    memo: NodeMemo,
}

/// 参照ファイル 1 つ分の、ノードごとの判定のメモ (キーはノード id)。同じブロックやスコープの
/// 中の出現ごとに兄弟や部分木を走査し直さず、出現ごとの処理を祖先の深さに比例する分に抑える。
#[derive(Default)]
struct NodeMemo {
    attributes: AttributeSummaries,
    /// ノード → そこから祖先までに、トークンを書き換え得るものが無いか (`rewrite_free`)。
    rewrite_free: RefCell<HashMap<usize, bool>>,
    /// ブロック → その文が束縛し得る名前。
    blocks: RefCell<HashMap<usize, Rc<BlockBindings>>>,
    /// 引数・パターンの部分木 → そこに現れる識別子。
    identifiers: RefCell<HashMap<usize, Rc<HashSet<String>>>>,
    /// `if`・`while` の条件 → `let` のパターンに現れる識別子。None はパターンを読めない
    /// `let` があり、どの名前も束縛し得る。
    conditions: RefCell<HashMap<usize, Option<Rc<HashSet<String>>>>>,
    /// 型引数を持ち得る item → 型引数の名前と、型か (`true`)、const か (`false`)。
    generics: RefCell<HashMap<usize, Rc<HashMap<String, bool>>>>,
}

impl NodeMemo {
    fn for_tree(root: Node<'_>, source: &[u8], policy: AttributePolicy) -> Self {
        Self {
            attributes: AttributeSummaries::collect(root, source, policy),
            ..Self::default()
        }
    }
    /// 参照を含む item とスコープに、トークンを書き換え得る属性やマクロの token tree が無いか。
    /// use 宣言と可視性の中の出現も扱わない。辿った祖先ごとに結果を覚える。
    fn rewrite_free(&self, node: Node<'_>, _source: &[u8], _policy: AttributePolicy) -> bool {
        let mut memo = self.rewrite_free.borrow_mut();
        let mut visited = Vec::new();
        let mut current = Some(node);
        // 辿ったノードの結果は、途中で書き換え得るものに当たれば偽、覚えた祖先に当たれば
        // その結果、ファイルの根まで何も無ければ真。
        let free = loop {
            let Some(here) = current else {
                break true;
            };
            if let Some(known) = memo.get(&here.id()) {
                break *known;
            }
            visited.push(here.id());
            if !locally_rewrite_free(here, &self.attributes) {
                break false;
            }
            current = here.parent();
        };
        for id in visited {
            memo.insert(id, free);
        }
        free
    }

    /// 部分木に `name` の識別子があるか (束縛の位置を絞らない、保守側の判定)。
    fn mentions(&self, node: Node<'_>, source: &[u8], name: &str) -> bool {
        self.identifiers
            .borrow_mut()
            .entry(node.id())
            .or_insert_with(|| Rc::new(identifiers_in(node, source)))
            .contains(name)
    }

    /// `if let`・`while let` の条件のパターンに `name` があるか。
    fn condition_binds(&self, condition: Node<'_>, source: &[u8], name: &str) -> bool {
        self.conditions
            .borrow_mut()
            .entry(condition.id())
            .or_insert_with(|| condition_identifiers(condition, source).map(Rc::new))
            .as_ref()
            .is_none_or(|names| names.contains(name))
    }

    /// item の型引数の並びで `name` を宣言しているか (`Some(true)` は型、`Some(false)` は const)。
    fn generic_parameter(&self, item: Node<'_>, source: &[u8], name: &str) -> Option<bool> {
        self.generics
            .borrow_mut()
            .entry(item.id())
            .or_insert_with(|| Rc::new(generic_parameters(item, source)))
            .get(name)
            .copied()
    }

    fn block(&self, block: Node<'_>, source: &[u8], policy: AttributePolicy) -> Rc<BlockBindings> {
        self.blocks
            .borrow_mut()
            .entry(block.id())
            .or_insert_with(|| Rc::new(BlockBindings::collect(block, source, policy)))
            .clone()
    }
}

/// ブロックの文が束縛し得る名前。item と use はブロック全体へ巻き上がり、文の位置の
/// マクロ・derive・未知の属性は未知の item を作り得る。
#[derive(Default)]
struct BlockBindings {
    /// 文の位置のマクロ・inner attribute・名前の解決を変え得る属性・グロブの import があり、
    /// どの名前も束縛し得る。
    opaque: bool,
    /// let のパターンに現れる識別子 (値の名前空間)。
    values: HashSet<String>,
    /// パターンを読めない let があり、値の名前空間のどの名前も束縛し得る。
    any_value: bool,
    /// item の名前と、use・extern crate・foreign mod の本体に現れる識別子 (両方の名前空間)。
    names: HashSet<String>,
}

impl BlockBindings {
    fn collect(block: Node<'_>, source: &[u8], policy: AttributePolicy) -> Self {
        let mut bindings = Self::default();
        let mut cursor = block.walk();
        for statement in block.named_children(&mut cursor) {
            match statement.kind() {
                "let_declaration" => match statement.child_by_field_name("pattern") {
                    Some(pattern) => bindings.values.extend(identifiers_in(pattern, source)),
                    None => bindings.any_value = true,
                },
                "expression_statement" => {
                    bindings.opaque |= statement
                        .named_child(0)
                        .is_some_and(|child| child.kind() == "macro_invocation");
                }
                "macro_invocation" | "inner_attribute_item" => bindings.opaque = true,
                "attribute_item" => {
                    bindings.opaque |=
                        !attribute_allowed(statement, source, |name| policy.keeps_item(name));
                }
                "use_declaration" | "extern_crate_declaration" => {
                    bindings.opaque |= statement
                        .utf8_text(source)
                        .is_ok_and(|text| text.contains('*'));
                    bindings.names.extend(identifier_tokens(statement, source));
                }
                "foreign_mod_item" => {
                    if let Some(body) = statement.child_by_field_name("body") {
                        bindings.names.extend(identifiers_in(body, source));
                    }
                }
                kind if kind.ends_with("_item") => {
                    bindings
                        .names
                        .extend(declared_name(statement, source).map(str::to_owned));
                }
                _ => {}
            }
        }
        bindings.opaque |= bindings.names.iter().any(|name| !name.is_ascii());
        bindings
    }

    fn may_bind(&self, name: &str, namespace: Namespace) -> bool {
        self.opaque
            || self.names.contains(name)
            || (namespace == Namespace::Value && (self.any_value || self.values.contains(name)))
    }
}

/// 1 回の削除判定の間、Cargo 入力とモジュール出現を共有する。
pub(crate) struct RustPathResolver {
    dir: PathBuf,
    layout: OnceCell<Option<layout::Layout>>,
    dependencies: dependencies::DependencyProof,
}

impl RustPathResolver {
    pub(crate) fn new(dir: &str) -> Self {
        Self {
            dir: PathBuf::from(dir),
            layout: OnceCell::new(),
            dependencies: dependencies::DependencyProof::new(std::path::Path::new(dir)),
        }
    }

    /// `ref_path` の解析木で、`name` の出現のうち残る項目を指すと確かめられた位置。
    pub(super) fn positions(
        &mut self,
        root: Node<'_>,
        source: &[u8],
        name: &str,
        ref_path: &str,
    ) -> HashMap<(usize, usize), PathSite> {
        let mut positions = HashMap::new();
        let mut ids = HashMap::new();
        let Some(scopes) = FileScopes::collect(root, source, Some(&mut ids)) else {
            return positions;
        };
        let file = RefFile {
            path: Rc::from(crate::git_support::normalize_workspace_separators(ref_path)),
            memo: NodeMemo::for_tree(root, source, scopes.policy),
            scopes: Rc::new(scopes),
            ids,
        };
        let _: std::ops::ControlFlow<()> = crate::engine::tree_walk::walk_preorder(
            root,
            crate::engine::tree_walk::Children::Named,
            |node, _| {
                if node.kind() == "identifier"
                    && node.utf8_text(source).is_ok_and(|text| text == name)
                    && let Some(site) = self.classify(&file, node, source, name)
                {
                    let point = node.start_position();
                    positions.insert((point.row, point.column), site);
                }
                std::ops::ControlFlow::Continue(crate::engine::tree_walk::Walk::Descend)
            },
        );
        positions
    }

    fn classify(
        &self,
        file: &RefFile,
        node: Node<'_>,
        source: &[u8],
        name: &str,
    ) -> Option<PathSite> {
        // 配置に依存しない証拠は、Cargo 入力の監査が使えない場合にも有効。
        if let Some(site) = self.classify_in(file, node, source, name, None) {
            return Some(site);
        }
        let layout = self.layout()?;
        let contexts = layout.contexts(&file.path)?;
        let mut proven = None;
        for &context in contexts {
            let site = self.classify_in(file, node, source, name, Some(context))?;
            if proven.as_ref().is_some_and(|known| *known != site) {
                return None;
            }
            proven = Some(site);
        }
        proven
    }

    fn classify_in(
        &self,
        file: &RefFile,
        node: Node<'_>,
        source: &[u8],
        name: &str,
        context: Option<usize>,
    ) -> Option<PathSite> {
        let parent = node.parent()?;
        let policy = file.scopes.policy;
        if matches!(parent.kind(), "lifetime" | "label")
            || !file.memo.rewrite_free(node, source, policy)
        {
            return None;
        }
        if parent.kind() == "scoped_identifier" {
            // マクロ名と、さらに長いパスの途中の段 (`a::run::b`) は関数を指さないので扱わない。
            let inner_segment = parent.parent().is_some_and(|outer| {
                matches!(
                    outer.kind(),
                    "macro_invocation" | "scoped_identifier" | "scoped_type_identifier"
                ) && outer
                    .child_by_field_name(if outer.kind() == "macro_invocation" {
                        "macro"
                    } else {
                        "path"
                    })
                    .is_some_and(|child| child.id() == parent.id())
            });
            if inner_segment
                || parent
                    .child_by_field_name("name")
                    .is_none_or(|last| last.id() != node.id())
            {
                return None;
            }
            return match self.resolve_qualifier(
                file,
                parent.child_by_field_name("path")?,
                source,
                context,
            )? {
                Resolved::Type => Some(PathSite::Associated),
                Resolved::Module(module) => self
                    .module_scopes(file, &module)?
                    .scopes
                    .get(module.scope)?
                    .declares_function(name)
                    .then(|| PathSite::ModuleFunction(module.file.clone())),
            };
        }
        if parent
            .child_by_field_name("name")
            .is_some_and(|declared| declared.id() == node.id())
        {
            // モジュール直下の自由関数の定義。有無が属性に左右されないものだけ。
            let scope = parent.parent()?;
            return (parent.kind() == "function_item"
                && is_module_scope(scope)
                && file
                    .scopes
                    .scopes
                    .get(*file.ids.get(&scope.id())?)?
                    .declares_function(name))
            .then(|| PathSite::ModuleFunction(file.path.clone()));
        }
        if !crate::engine::refs::is_rust_shadowable_value_identifier(node) {
            return None;
        }
        let Lexical::Module(scope) =
            lexical_scope(node, source, name, Namespace::Value, policy, &file.memo)?
        else {
            return None;
        };
        file.scopes
            .scopes
            .get(*file.ids.get(&scope.id())?)?
            .declares_function(name)
            .then(|| PathSite::ModuleFunction(file.path.clone()))
    }

    /// 修飾部が指すモジュールか型。確かめられなければ None。
    fn resolve_qualifier(
        &self,
        file: &RefFile,
        qualifier: Node<'_>,
        source: &[u8],
        context: Option<usize>,
    ) -> Option<Resolved> {
        match qualifier.kind() {
            // `<T>::name`・`<T as Trait>::name` は型の関連項目だけを指す。
            "bracketed_type" => Some(Resolved::Type),
            // 型引数を誤って付けたモジュールのパスも構文としては現れるので、型引数を除いた部分が
            // 型と確かめられたときだけ型とする。
            "generic_type" => {
                match self.resolve_path(
                    file,
                    qualifier.child_by_field_name("type")?,
                    source,
                    context,
                )? {
                    Resolved::Type => Some(Resolved::Type),
                    Resolved::Module(_) => None,
                }
            }
            "identifier" if qualifier.utf8_text(source).ok() == Some("Self") => {
                let mut ancestor = qualifier.parent();
                while let Some(node) = ancestor {
                    if matches!(node.kind(), "impl_item" | "trait_item") {
                        return Some(Resolved::Type);
                    }
                    ancestor = node.parent();
                }
                None
            }
            _ => self.resolve_path(file, qualifier, source, context),
        }
    }

    fn resolve_path(
        &self,
        file: &RefFile,
        path: Node<'_>,
        source: &[u8],
        context: Option<usize>,
    ) -> Option<Resolved> {
        let (segments, global) = path_segments(path)?;
        let (first, rest) = segments.split_first()?;
        let mut current = match first.kind() {
            "identifier" if global => {
                self.dependency_root(file, plain_name(first.utf8_text(source).ok()?), context)?
            }
            "crate" => self.layout()?.crate_root(context?),
            "self" => current_module(file, *first)?,
            "super" => {
                let module = current_module(file, *first)?;
                self.parent_module(file, &module, context)?
            }
            "identifier" | "type_identifier" => {
                let name = plain_name(first.utf8_text(source).ok()?);
                match lexical_scope(
                    *first,
                    source,
                    name,
                    Namespace::Type,
                    file.scopes.policy,
                    &file.memo,
                )? {
                    Lexical::GenericType => return rest.is_empty().then_some(Resolved::Type),
                    Lexical::Module(scope) => {
                        let module = ModuleRef {
                            file: file.path.clone(),
                            scope: *file.ids.get(&scope.id())?,
                            occurrence: None,
                        };
                        let facts = file.scopes.scopes.get(module.scope)?;
                        if facts.lacks_type_name(name) {
                            if self.implicit_prelude_disabled(file, &module, context)? {
                                return None;
                            }
                            self.dependency_root(file, name, context)?
                        } else {
                            match self.step(file, &module, name, rest.is_empty(), context)? {
                                Resolved::Type => return Some(Resolved::Type),
                                Resolved::Module(module) => module,
                            }
                        }
                    }
                }
            }
            _ => return None,
        };
        let mut leading = matches!(first.kind(), "self" | "super");
        for (index, segment) in rest.iter().enumerate() {
            match segment.kind() {
                "super" if leading => current = self.parent_module(file, &current, context)?,
                "identifier" | "type_identifier" => {
                    leading = false;
                    let name = plain_name(segment.utf8_text(source).ok()?);
                    match self.step(file, &current, name, index + 1 == rest.len(), context)? {
                        Resolved::Type => return Some(Resolved::Type),
                        Resolved::Module(module) => current = module,
                    }
                }
                _ => return None,
            }
        }
        Some(Resolved::Module(current))
    }

    /// モジュールの直接宣言だけを使う。ファイル境界には監査済みの所属が必要。
    fn step(
        &self,
        file: &RefFile,
        module: &ModuleRef,
        name: &str,
        last: bool,
        context: Option<usize>,
    ) -> Option<Resolved> {
        let scopes = self.module_scopes(file, module)?;
        let facts = scopes.scopes.get(module.scope)?;
        if last && facts.declares_type(name) {
            return Some(Resolved::Type);
        }
        match facts.child_module(name)? {
            ChildModule::Inline(scope) if module.occurrence.is_none() => {
                Some(Resolved::Module(ModuleRef {
                    file: module.file.clone(),
                    scope,
                    occurrence: None,
                }))
            }
            _ => {
                let layout = self.layout()?;
                let occurrence = module
                    .occurrence
                    .or_else(|| layout.scope_occurrence(context?, module.scope))?;
                Some(Resolved::Module(layout.child(occurrence, name)?))
            }
        }
    }

    fn parent_module(
        &self,
        file: &RefFile,
        module: &ModuleRef,
        context: Option<usize>,
    ) -> Option<ModuleRef> {
        if module.occurrence.is_none() {
            let scopes = self.module_scopes(file, module)?;
            if let Some(parent) = scopes.scopes.get(module.scope)?.parent {
                return Some(ModuleRef {
                    scope: parent,
                    ..module.clone()
                });
            }
        }
        let layout = self.layout()?;
        let occurrence = module
            .occurrence
            .or_else(|| layout.scope_occurrence(context?, module.scope))?;
        layout.parent(occurrence)
    }

    fn module_scopes(&self, file: &RefFile, module: &ModuleRef) -> Option<Rc<FileScopes>> {
        if module.file == file.path {
            return Some(file.scopes.clone());
        }
        self.layout()?.file_scopes(&module.file)
    }

    fn implicit_prelude_disabled(
        &self,
        file: &RefFile,
        module: &ModuleRef,
        context: Option<usize>,
    ) -> Option<bool> {
        let layout = self.layout()?;
        let root = layout.crate_root(context?).occurrence?;
        let mut current = module.clone();
        loop {
            let scopes = self.module_scopes(file, &current)?;
            if scopes.scopes.get(current.scope)?.no_implicit_prelude {
                return Some(true);
            }
            let occurrence = current
                .occurrence
                .or_else(|| layout.scope_occurrence(context?, current.scope))?;
            if occurrence == root {
                return Some(false);
            }
            current = self.parent_module(file, &current, context)?;
        }
    }

    fn dependency_root(
        &self,
        file: &RefFile,
        name: &str,
        context: Option<usize>,
    ) -> Option<ModuleRef> {
        let layout = self.layout()?;
        let root = layout.crate_root(context?);
        let scopes = self.module_scopes(file, &root)?;
        let facts = scopes.scopes.get(root.scope)?;
        // root の展開は extern prelude 自体を変え得る。`::name` でもこの確認は要る。
        if facts.opaque
            || facts.unknown_children
            || facts.may_generate
            || facts.extern_crates.contains(name)
        {
            return None;
        }
        self.dependencies.root(layout, context?, name)
    }

    fn layout(&self) -> Option<&layout::Layout> {
        self.layout
            .get_or_init(|| layout::Layout::build(&self.dir))
            .as_ref()
    }
}

fn current_module(file: &RefFile, node: Node<'_>) -> Option<ModuleRef> {
    let mut ancestor = node.parent();
    while let Some(scope) = ancestor {
        if let Some(index) = file.ids.get(&scope.id()) {
            return Some(ModuleRef {
                file: file.path.clone(),
                scope: *index,
                occurrence: None,
            });
        }
        // 関数の本体などに置いた mod は登録していない。その境界を越えて外側のモジュールを
        // `self`・`super` の起点にしない。
        if is_module_scope(scope) {
            return None;
        }
        ancestor = scope.parent();
    }
    None
}

/// パスの各段のノードと、extern prelude だけを引く `::` で始まるか。
fn path_segments(node: Node<'_>) -> Option<(Vec<Node<'_>>, bool)> {
    let mut reversed = Vec::new();
    let mut current = node;
    let global = loop {
        match current.kind() {
            "identifier" | "type_identifier" | "crate" | "self" | "super" => {
                reversed.push(current);
                break false;
            }
            "scoped_identifier" | "scoped_type_identifier" => {
                let name = current.child_by_field_name("name")?;
                if !matches!(name.kind(), "identifier" | "type_identifier" | "super") {
                    return None;
                }
                reversed.push(name);
                match current.child_by_field_name("path") {
                    Some(path) => current = path,
                    None if current.child(0).is_some_and(|token| token.kind() == "::") => {
                        break true;
                    }
                    None => return None,
                }
            }
            _ => return None,
        }
    };
    reversed.reverse();
    Some((reversed, global))
}

/// 部分木に現れる識別子 (束縛の位置を絞らない、保守側の材料)。
fn identifiers_in(node: Node<'_>, source: &[u8]) -> HashSet<String> {
    let mut found = HashSet::new();
    let mut pending = vec![node];
    while let Some(current) = pending.pop() {
        if matches!(
            current.kind(),
            "identifier" | "shorthand_field_identifier" | "type_identifier"
        ) && let Ok(text) = current.utf8_text(source)
        {
            found.insert(plain_name(text).to_owned());
        }
        let mut cursor = current.walk();
        pending.extend(current.named_children(&mut cursor));
    }
    found
}

/// `if`・`while` の条件の `let` (let_chain の中も) のパターンに現れる識別子。パターンを読めない
/// `let` があれば None。
fn condition_identifiers(condition: Node<'_>, source: &[u8]) -> Option<HashSet<String>> {
    let mut found = HashSet::new();
    let mut pending = vec![condition];
    while let Some(current) = pending.pop() {
        match current.kind() {
            "let_condition" => found.extend(identifiers_in(
                current.child_by_field_name("pattern")?,
                source,
            )),
            "let_chain" => {
                let mut cursor = current.walk();
                pending.extend(current.named_children(&mut cursor));
            }
            _ => {}
        }
    }
    Some(found)
}

/// item の型引数の並び。名前ごとに最初の宣言が型か (`true`)、const か (`false`)。
fn generic_parameters(item: Node<'_>, source: &[u8]) -> HashMap<String, bool> {
    let mut found = HashMap::new();
    let Some(parameters) = item.child_by_field_name("type_parameters") else {
        return found;
    };
    let mut cursor = parameters.walk();
    for parameter in parameters.named_children(&mut cursor) {
        if let Some(name) = declared_name(parameter, source) {
            found
                .entry(name.to_owned())
                .or_insert(parameter.kind() != "const_parameter");
        }
    }
    found
}

/// 参照から最も内側のモジュールまでの間で `name` を束縛し得るものが無ければ、そのモジュールの
/// スコープを返す。見えている総称型の型引数なら `GenericType`。
fn lexical_scope<'t>(
    node: Node<'t>,
    source: &[u8],
    name: &str,
    namespace: Namespace,
    policy: AttributePolicy,
    memo: &NodeMemo,
) -> Option<Lexical<'t>> {
    let mentions = |node: Node<'_>| memo.mentions(node, source, name);
    let mut child = node;
    // 外側の item の型引数は内側の item から見えない (関連関数は impl・trait のものを見る)。
    let mut generics_visible = true;
    while let Some(parent) = child.parent() {
        if is_module_scope(parent) {
            return Some(Lexical::Module(parent));
        }
        match parent.kind() {
            "block" if memo.block(parent, source, policy).may_bind(name, namespace) => {
                return None;
            }
            "function_item" | "function_signature_item" => {
                if namespace == Namespace::Value
                    && parent
                        .child_by_field_name("parameters")
                        .is_some_and(mentions)
                {
                    return None;
                }
                if generics_visible
                    && let Some(is_type) = memo.generic_parameter(parent, source, name)
                {
                    return (is_type && namespace == Namespace::Type)
                        .then_some(Lexical::GenericType);
                }
                generics_visible = parent.parent().is_some_and(|body| {
                    body.kind() == "declaration_list"
                        && body
                            .parent()
                            .is_some_and(|owner| matches!(owner.kind(), "impl_item" | "trait_item"))
                });
            }
            "impl_item" | "trait_item" | "struct_item" | "enum_item" | "union_item"
            | "type_item" => {
                if generics_visible
                    && let Some(is_type) = memo.generic_parameter(parent, source, name)
                {
                    return (is_type && namespace == Namespace::Type)
                        .then_some(Lexical::GenericType);
                }
                generics_visible = false;
            }
            "closure_expression"
                if namespace == Namespace::Value
                    && parent
                        .child_by_field_name("parameters")
                        .is_some_and(mentions) =>
            {
                return None;
            }
            "match_arm" | "for_expression" | "let_declaration" | "let_condition"
                if namespace == Namespace::Value
                    && parent.child_by_field_name("pattern").is_none_or(mentions) =>
            {
                return None;
            }
            "if_expression" | "while_expression"
                if namespace == Namespace::Value
                    && parent
                        .child_by_field_name("condition")
                        .is_some_and(|condition| memo.condition_binds(condition, source, name)) =>
            {
                return None;
            }
            _ => {}
        }
        child = parent;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 末尾の `run` の分類。配置を確かめられない場所に置くので、ファイル内で閉じる証明だけが効く。
    fn last_site(source: &str) -> Option<PathSite> {
        let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
        assert!(!tree.root_node().has_error(), "{source}");
        let start = source.rfind("run").unwrap();
        let point = tree
            .root_node()
            .descendant_for_byte_range(start, start + 3)
            .unwrap()
            .start_position();
        let dir = tempfile::tempdir().unwrap();
        RustPathResolver::new(dir.path().to_str().unwrap())
            .positions(tree.root_node(), source.as_bytes(), "run", "src/sample.rs")
            .remove(&(point.row, point.column))
    }

    #[test]
    fn removed_rust_paths_resolve_local_functions_and_types() {
        let local = Some(PathSite::ModuleFunction(Rc::from("src/sample.rs")));
        for source in [
            "fn run() {} fn f() { run(); }",
            "fn run() {} fn f() { let g = run; }",
            "fn run() {} fn f() { run::<u8>(); }",
            "fn run() {} fn f() { S { run }; }",
            "fn run() {} fn f() { self::run(); }",
            "fn run() {} mod m { fn f() { super::run(); } }",
            "mod m { pub fn run() {} } fn f() { m::run(); }",
            "mod m { pub mod n { pub fn run() {} } } fn f() { self::m::n::run(); }",
            "#[inline] fn run() {} fn f() { run(); }",
            "#[cfg_attr(test, inline)] fn run() {} fn f() { run(); }",
            "#![allow(dead_code)] fn run() {} fn f() { run(); }",
            "fn run() {} #[cfg(test)] fn f() { run(); }",
            "fn run() {} fn f() { let x = 1; println!(\"{}\", x); }\nfn g() { run(); }",
            "fn run() {} fn f() { let other = 1; { run(); } }",
            "fn run() {} impl S { fn run(&self) {} } fn f() { run(); }",
            "pub fn run() {}",
            "mod m { pub fn run() {} }",
        ] {
            assert_eq!(last_site(source), local, "{source}");
        }
        for source in [
            "struct E; fn f() { E::run(); }",
            "struct E { x: u8 } fn f() { E::run(); }",
            "enum E { A } fn f() { E::run(); }",
            "trait E { fn run(); } fn f() { E::run(); }",
            "type E = u8; fn f() { E::run(); }",
            "#[derive(Debug, Clone)] struct E; fn f() { E::run(); }",
            "#[cfg_attr(test, derive(Debug))] struct E; fn f() { E::run(); }",
            "struct E; impl E { fn f() { Self::run(); } }",
            "trait T { fn f() { Self::run(); } }",
            "fn f() { <E>::run(); }",
            "fn f() { <E as Runner>::run(); }",
            "struct E; fn f() { E::<u8>::run(); }",
            "mod m { pub struct E; } fn f() { m::E::run(); }",
            "fn f<T: Runner>() { T::run(); }",
            "struct S<T>(T); impl<T: Runner> S<T> { fn f() { T::run(); } }",
        ] {
            assert_eq!(last_site(source), Some(PathSite::Associated), "{source}");
        }
    }

    /// 束縛し得るもの・書き換え得るもの・宣言の無い名前は分類しない。
    #[test]
    fn removed_rust_paths_keep_unproven_shapes() {
        for source in [
            // 宣言が無い・同名の値が複数・import・条件付きの宣言・書き換え得る属性
            "fn f() { run(); }",
            "fn run() {} const run: u8 = 1; fn f() { run(); }",
            "fn run() {} use other::run; fn f() { run(); }",
            "#[cfg(test)] fn run() {} fn f() { run(); }",
            "#[rewrite] fn run() {} fn f() { run(); }",
            "fn run() {} #[rewrite] fn f() { run(); }",
            "fn run() {} #[tokio::main] fn f() { run(); }",
            "fn run() {} #[cfg_attr(test, tokio::main)] fn f() { run(); }",
            "#![rewrite] fn run() {} fn f() { run(); }",
            "fn run() {} fn f() { #![rewrite] run(); }",
            // マクロの中・use の中・マクロ名・長いパスの途中の段
            "fn run() {} fn f() { m!(run()); }",
            "fn run() {} fn f() { crate::m::run!(); }",
            "mod m { pub fn run() {} } use m::run;",
            "mod m { pub fn run() {} pub mod run { pub fn go() {} } } fn f() { m::run::go(); }",
            // ブロック・関数・クロージャ・パターンの束縛
            "fn run() {} fn f() { let run = || 1; run(); }",
            "fn run() {} fn f(run: fn()) { run(); }",
            "fn run() {} fn f() { let g = |run: fn()| run(); }",
            "fn run() {} fn f() { match x { Some(run) => run(), _ => {} } }",
            "fn run() {} fn f() { if let Some(run) = x { run(); } }",
            "fn run() {} fn f() { for run in xs { run(); } }",
            "fn run() {} fn f<const run: usize>() { run; }",
            "fn run() {} fn f() { fn run() {} run(); }",
            "fn run() {} fn f() { use other::*; run(); }",
            "fn run() {} fn f() { make_items!(); run(); }",
            "fn run() {} fn f() { #[derive(Inject)] struct Marker; run(); }",
            "fn run() {} fn f() { #[rewrite] struct Marker; run(); }",
            // 先頭が通常の名前のパスも、内側の束縛 (呼び出しより後の import を含む) を確かめる
            "mod engine { pub fn run() {} } fn f() { use crate::launcher as engine; engine::run(); }",
            "mod engine { pub fn run() {} } fn f() { engine::run(); use crate::launcher as engine; }",
            "struct E; fn f() { use crate::launcher as E; E::run(); }",
            "fn f<T>() { fn g() { T::run(); } }",
            // 型と確かめられない修飾部
            "fn f() { E::run(); }",
            "fn f() { launcher::<u8>::run(); }",
            "mod launcher {} fn f() { launcher::<u8>::run(); }",
            "#[rewrite] struct E; fn f() { E::run(); }",
            "#[cfg(test)] struct E; fn f() { E::run(); }",
            "use my::derive; #[derive(Debug)] struct E; fn f() { E::run(); }",
            "struct E; mod E {} fn f() { E::run(); }",
            "fn f() { Self::run(); }",
            "fn f() { ::run(); }",
            // ファイルをまたぐ解決は、配置を確かめられない場所では行わない
            "fn run() {} fn f() { crate::run(); }",
            "mod m; fn f() { m::run(); }",
            "fn f() { super::run(); }",
            // ラベルは値ではない
            "fn run() {} fn f() { 'run: loop {} }",
        ] {
            assert_eq!(last_site(source), None, "{source}");
        }
    }

    #[test]
    fn removed_rust_paths_read_cfg_attr_contents() {
        for (source, expected) in [
            ("#[cfg_attr(test, inline)] fn f() {}", Some(vec!["inline"])),
            (
                "#[cfg_attr(all(a, b), derive(Debug), doc = \"x\",)] fn f() {}",
                Some(vec!["derive", "doc"]),
            ),
            ("#[cfg_attr(test, tokio::main)] fn f() {}", None),
            ("#[tokio::main] fn f() {}", None),
            ("#[inline] fn f() {}", Some(vec!["inline"])),
        ] {
            let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
            let attribute = tree.root_node().named_child(0).unwrap();
            assert_eq!(
                attribute_names(attribute, source.as_bytes()),
                expected,
                "{source}"
            );
        }
    }

    /// `files` を置いたリポジトリで、`ref_path` の末尾の `run` の分類。
    pub(super) fn site_in(files: &[(&str, &str)], ref_path: &str) -> Option<PathSite> {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        for (path, source) in files {
            let full = dir.path().join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, source).unwrap();
        }
        let source = files
            .iter()
            .rev()
            .find(|(path, _)| *path == ref_path)
            .unwrap()
            .1;
        let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
        let start = source.rfind("run").unwrap();
        let point = tree
            .root_node()
            .descendant_for_byte_range(start, start + 3)
            .unwrap()
            .start_position();
        RustPathResolver::new(dir.path().to_str().unwrap())
            .positions(tree.root_node(), source.as_bytes(), "run", ref_path)
            .remove(&(point.row, point.column))
    }

    pub(super) const MANIFEST: (&str, &str) = (
        "Cargo.toml",
        "[package]\nname = \"tool\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );

    #[test]
    fn removed_rust_paths_audit_ignored_cargo_inputs() {
        let base = [
            MANIFEST,
            (".gitignore", "tests/\n"),
            ("src/lib.rs", "pub mod engine; pub mod app;"),
            ("src/engine.rs", "pub fn run() {}"),
            ("src/app.rs", "pub fn f() { super::engine::run(); }"),
        ];
        for root in ["tests/local.rs", "tests/probe/main.rs"] {
            let mut files = base.to_vec();
            files.push((root, "#[path = \"../src/app.rs\"] mod mirror;"));
            assert_eq!(site_in(&files, "src/app.rs"), None, "{root}");
        }
    }

    #[test]
    fn removed_rust_paths_accept_complete_root_memberships() {
        for (manifest, root) in [
            ("[lib]\npath = \"entry\"\n", "entry"),
            (
                "[[bin]]\nname = \"probe\"\npath = \"entry.入口\"\n",
                "entry.入口",
            ),
            ("", "build.rs"),
            ("", "tests/probe.rs"),
            ("", "examples/probe/main.rs"),
            ("", "benches/probe.rs"),
            ("", "src/bin/probe/main.rs"),
        ] {
            let manifest = format!("{}{manifest}", MANIFEST.1);
            let directory = root.rsplit_once('/').map_or("", |(dir, _)| dir);
            let engine = if directory.is_empty() {
                "engine.rs".to_owned()
            } else {
                format!("{directory}/engine.rs")
            };
            assert_eq!(
                site_in(
                    &[
                        ("Cargo.toml", &manifest),
                        (root, "mod engine; pub fn f() { crate::engine::run(); }"),
                        (&engine, "pub fn run() {}")
                    ],
                    root
                ),
                Some(PathSite::ModuleFunction(Rc::from(engine.as_str()))),
                "{root}",
            );
        }
        // 同じファイルでも crate root と通常 mod では子の探索先が異なる。
        assert_eq!(
            site_in(
                &[
                    MANIFEST,
                    (
                        "src/lib.rs",
                        "pub mod engine; pub fn f() { self::engine::run(); }"
                    ),
                    ("src/main.rs", "mod lib; fn main() {}"),
                    ("src/engine.rs", "pub fn run() {}"),
                    ("src/lib/engine.rs", "pub fn run() {}"),
                ],
                "src/lib.rs"
            ),
            None
        );
    }

    #[test]
    fn removed_rust_paths_follow_cargo_module_files() {
        let engine = Some(PathSite::ModuleFunction(Rc::from("src/engine.rs")));
        for (files, ref_path, expected) in [
            (
                vec![
                    MANIFEST,
                    ("src/lib.rs", "pub mod engine;\npub mod app;\n"),
                    ("src/engine.rs", "pub fn run() {}\n"),
                    ("src/app.rs", "pub fn f() { crate::engine::run(); }\n"),
                ],
                "src/app.rs",
                engine.clone(),
            ),
            (
                vec![
                    MANIFEST,
                    ("src/lib.rs", "pub mod engine;\npub mod app;\n"),
                    ("src/engine.rs", "pub fn run() {}\n"),
                    ("src/app.rs", "pub fn f() { super::engine::run(); }\n"),
                ],
                "src/app.rs",
                engine.clone(),
            ),
            (
                vec![
                    MANIFEST,
                    (
                        "src/lib.rs",
                        "pub mod engine;\npub fn f() { engine::run(); }\n",
                    ),
                    ("src/engine.rs", "pub fn run() {}\n"),
                ],
                "src/lib.rs",
                engine.clone(),
            ),
            (
                vec![
                    MANIFEST,
                    (
                        "src/main.rs",
                        "mod engine;\nfn main() { crate::engine::run(); }\n",
                    ),
                    ("src/engine.rs", "pub fn run() {}\n"),
                ],
                "src/main.rs",
                engine.clone(),
            ),
            // インライン mod の中のファイル mod と mod.rs
            (
                vec![
                    MANIFEST,
                    ("src/lib.rs", "pub mod a { pub mod engine; pub mod b; }\n"),
                    ("src/a/engine/mod.rs", "pub fn run() {}\n"),
                    ("src/a/b.rs", "pub fn f() { super::engine::run(); }\n"),
                ],
                "src/a/b.rs",
                Some(PathSite::ModuleFunction(Rc::from("src/a/engine/mod.rs"))),
            ),
            (
                vec![
                    MANIFEST,
                    ("src/lib.rs", "pub mod engine;\npub mod app;\n"),
                    ("src/engine.rs", "pub struct Engine;\n"),
                    (
                        "src/app.rs",
                        "pub fn f() { crate::engine::Engine::run(); }\n",
                    ),
                ],
                "src/app.rs",
                Some(PathSite::Associated),
            ),
        ] {
            assert_eq!(site_in(&files, ref_path), expected, "{files:?}");
        }
    }

    /// 別所属を解決できない場合や、複数の所属で証拠が食い違う場合は分類しない。
    #[test]
    fn removed_rust_paths_reject_other_compilation_contexts() {
        let base = [
            MANIFEST,
            ("src/lib.rs", "pub mod engine;\npub mod app;\n"),
            ("src/engine.rs", "pub fn run() {}\n"),
            ("src/app.rs", "pub fn f() { crate::engine::run(); }\n"),
        ];
        for extra in [
            // 暗黙の build script・bin からの mod 宣言・取り込み
            vec![(
                "build.rs",
                "#[path = \"src/app.rs\"] mod mirror; fn main() {}\n",
            )],
            vec![(
                "src/main.rs",
                "mod app; mod engine { pub fn run() {} } fn main() {}\n",
            )],
            vec![
                ("src/lib.rs", "pub mod engine; pub mod app; mod other;\n"),
                ("src/other.rs", "#[path = \"app.rs\"] mod mirror;\n"),
            ],
            vec![
                ("src/lib.rs", "pub mod engine; pub mod app; mod other;\n"),
                ("src/other.rs", "include!(\"app.rs\");\n"),
            ],
            vec![(
                "examples/demo.rs",
                "#[cfg_attr(all(), path = \"../src/app.rs\")] mod app;\nfn main() {}\n",
            )],
            // 明示パスのターゲット・宣言の重複・同名のファイル配置・条件付きの宣言
            vec![(
                "other/Cargo.toml",
                "[package]\nname = \"other\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[[bin]]\nname = \"x\"\npath = \"../src/x.rs\"\n",
            )],
            vec![("src/engine/mod.rs", "pub fn run() {}\n")],
            vec![(
                "src/lib.rs",
                "pub mod engine;\npub mod app;\npub mod engine;\n",
            )],
            vec![(
                "src/lib.rs",
                "#[cfg(test)]\npub mod engine;\npub mod app;\n",
            )],
            // 別の root が条件付きで同じファイルを mod にする (feature ごとの組み込み)
            vec![(
                "src/main.rs",
                "#[cfg(feature = \"cli\")]\nuse tool::launcher as engine;\n#[cfg(feature = \"cli\")]\nmod app;\nfn main() {}\n",
            )],
            vec![(
                "src/main.rs",
                "#[cfg_attr(feature = \"cli\", allow(unused))]\nmod app;\nfn main() {}\n",
            )],
            vec![(
                "src/lib.rs",
                "pub mod engine;\npub mod app;\nuse other::engine;\n",
            )],
            vec![("src/engine.rs", "#[cfg(test)]\npub fn run() {}\n")],
            vec![(
                "src/engine.rs",
                "pub fn run() {}\npub static run: u8 = 0;\n",
            )],
            vec![(
                "Cargo.toml",
                "[package]\nname = \"tool\"\nversion = \"0.1.0\"\nedition = \"2015\"\n",
            )],
        ] {
            let mut files: Vec<(&str, &str)> = base.to_vec();
            for (path, source) in &extra {
                files.retain(|(existing, _)| existing != path);
                files.push((path, source));
            }
            assert_eq!(site_in(&files, "src/app.rs"), None, "{extra:?}");
        }
        // 対照: 参照ファイルの祖先でない bin・テスト・example は root にならない
        let mut files = base.to_vec();
        files.extend([
            ("src/bin/tool.rs", "fn main() {}\n"),
            ("tests/it.rs", "#[test]\nfn works() {}\n"),
            ("examples/demo.rs", "fn main() {}\n"),
        ]);
        assert_eq!(
            site_in(&files, "src/app.rs"),
            Some(PathSite::ModuleFunction(Rc::from("src/engine.rs")))
        );
        // 対照: 空の build script・同じ証拠になる複数所属・到達しない scratch は妨げない。
        for extra in [
            vec![("build.rs", "fn main() {}")],
            vec![("src/main.rs", "mod engine; mod app; fn main() {}")],
            vec![("src/other.rs", "include!(\"app.rs\");")],
            vec![(
                "Cargo.toml",
                "[package]\nname = \"tool\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\npath = \"src/lib.rs\"\n",
            )],
        ] {
            let mut files = base.to_vec();
            for (path, source) in extra {
                files.retain(|(existing, _)| *existing != path);
                files.push((path, source));
            }
            assert_eq!(
                site_in(&files, "src/app.rs"),
                Some(PathSite::ModuleFunction(Rc::from("src/engine.rs"))),
                "{files:?}"
            );
        }
        // 別 package の src の下に置いた package と、テストの下のファイル
        for (files, ref_path) in [
            (
                vec![
                    MANIFEST,
                    ("src/lib.rs", "pub mod inner;\n"),
                    ("src/inner/Cargo.toml", MANIFEST.1),
                    ("src/inner/src/lib.rs", "pub mod engine;\npub mod app;\n"),
                    ("src/inner/src/engine.rs", "pub fn run() {}\n"),
                    (
                        "src/inner/src/app.rs",
                        "pub fn f() { crate::engine::run(); }\n",
                    ),
                ],
                "src/inner/src/app.rs",
            ),
            (
                vec![
                    MANIFEST,
                    ("src/lib.rs", "pub fn run() {}\n"),
                    ("tests/common/mod.rs", "pub fn f() { crate::run(); }\n"),
                ],
                "tests/common/mod.rs",
            ),
        ] {
            assert_eq!(site_in(&files, ref_path), None, "{files:?}");
        }
    }

    /// 関数の本体などに置いた mod は登録していないので、その中の `self`・`super` を外側の
    /// モジュールとして解決しない。
    #[test]
    fn removed_rust_paths_do_not_skip_block_modules() {
        for source in [
            "pub fn run() {}\npub fn drive() {\n    mod local {\n        use crate::launcher::*;\n        pub fn call() { self::run(); }\n    }\n    local::call();\n}\n",
            "pub fn run() {}\npub fn drive() {\n    mod local {\n        pub fn call() { super::run(); }\n    }\n}\n",
        ] {
            assert_eq!(last_site(source), None, "{source}");
        }
        // 対照: ファイル直下のインライン mod の `super` は解決する
        assert_eq!(
            last_site("pub fn run() {}\nmod local {\n    pub fn call() { super::run(); }\n}\n"),
            Some(PathSite::ModuleFunction(Rc::from("src/sample.rs")))
        );
    }

    /// `.gitignore` の対象で配置の走査が読まなかったソースが `src` の下にあれば、その中の
    /// `#[path]` などを確かめていないので、ファイルをまたいで解決しない。
    #[test]
    fn removed_rust_paths_reject_unscanned_sources_under_src() {
        let site = |files: &[(&str, &str)]| {
            let dir = tempfile::tempdir().unwrap();
            assert!(
                std::process::Command::new("git")
                    .args(["init", "-q"])
                    .current_dir(dir.path())
                    .status()
                    .unwrap()
                    .success()
            );
            for (path, source) in files {
                let full = dir.path().join(path);
                std::fs::create_dir_all(full.parent().unwrap()).unwrap();
                std::fs::write(full, source).unwrap();
            }
            let source = files
                .iter()
                .find(|(path, _)| *path == "src/app.rs")
                .unwrap()
                .1;
            let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
            let start = source.rfind("run").unwrap();
            let point = tree
                .root_node()
                .descendant_for_byte_range(start, start + 3)
                .unwrap()
                .start_position();
            RustPathResolver::new(dir.path().to_str().unwrap())
                .positions(tree.root_node(), source.as_bytes(), "run", "src/app.rs")
                .remove(&(point.row, point.column))
        };
        let base = [
            MANIFEST,
            ("src/engine.rs", "pub fn run() {}\n"),
            ("src/app.rs", "pub fn f() { super::engine::run(); }\n"),
        ];
        let mut ignored = base.to_vec();
        ignored.extend([
            (".gitignore", "src/generated.rs\n"),
            (
                "src/lib.rs",
                "pub mod engine;\npub mod app;\nmod generated;\n",
            ),
            (
                "src/generated.rs",
                "use crate::launcher as engine;\n#[path = \"app.rs\"]\nmod copy;\n",
            ),
        ]);
        assert_eq!(site(&ignored), None);
        // 対照: `src` の外の無視されたソースは mod 宣言で届かないので妨げない
        let mut outside = base.to_vec();
        outside.extend([
            (".gitignore", "notes/\n"),
            ("src/lib.rs", "pub mod engine;\npub mod app;\n"),
            ("notes/scratch.rs", "fn main() {}\n"),
        ]);
        assert_eq!(
            site(&outside),
            Some(PathSite::ModuleFunction(Rc::from("src/engine.rs")))
        );
    }

    #[test]
    fn removed_rust_paths_node_kinds_exist_in_the_grammar() {
        let language = LangId::Rust.ts_language();
        for kind in [
            "source_file",
            "declaration_list",
            "mod_item",
            "function_item",
            "function_signature_item",
            "const_item",
            "static_item",
            "struct_item",
            "enum_item",
            "union_item",
            "trait_item",
            "type_item",
            "impl_item",
            "foreign_mod_item",
            "associated_type",
            "use_declaration",
            "extern_crate_declaration",
            "field_declaration_list",
            "attribute_item",
            "inner_attribute_item",
            "attribute",
            "token_tree",
            "macro_definition",
            "macro_invocation",
            "visibility_modifier",
            "scoped_identifier",
            "scoped_type_identifier",
            "bracketed_type",
            "generic_type",
            "identifier",
            "type_identifier",
            "shorthand_field_identifier",
            "crate",
            "self",
            "super",
            "block",
            "expression_statement",
            "let_declaration",
            "let_condition",
            "let_chain",
            "match_arm",
            "for_expression",
            "if_expression",
            "while_expression",
            "closure_expression",
            "const_parameter",
            "lifetime",
            "label",
            "line_comment",
            "block_comment",
        ] {
            tree_sitter::Query::new(&language, &format!("({kind}) @node"))
                .unwrap_or_else(|error| panic!("{kind}: {error}"));
        }
        for query in [
            "(mod_item name: (_) body: (_))",
            "(scoped_identifier path: (_) name: (_))",
            "(generic_type type: (_))",
            "(attribute arguments: (_))",
            "(function_item type_parameters: (_) parameters: (_))",
            "(impl_item type_parameters: (_))",
            "(let_condition pattern: (_))",
            "(if_expression condition: (_))",
            "(while_expression condition: (_))",
            "(match_arm pattern: (_))",
            "(for_expression pattern: (_))",
            "(closure_expression parameters: (_))",
            "(foreign_mod_item body: (_))",
            "(struct_item body: (_))",
        ] {
            tree_sitter::Query::new(&language, query)
                .unwrap_or_else(|error| panic!("{query}: {error}"));
        }
    }
}
