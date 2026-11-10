//! 標準のマクロの呼び出しを、同じ意味の普通の式に書き換えたファイルの写し (Issue #310)。
//!
//! tree-sitter はマクロの引数を token tree (平たいトークンの列) にするので、引数の中の参照は
//! マクロの外と同じ所有元の証明を使えない。引数を式・パターンとして読む標準のマクロと
//! 証明できた呼び出しだけを普通の式に書き換えたファイル全体の写しを作り、写しの構文木で
//! 同じ証明を回す。写しは所有元の証明だけに使い、参照の位置・出力・ディスクのファイルとの
//! 比較には元のソースを使う。
//!
//! - `matches!(E, P)` / `matches!(E, P if G)` → `(match (E) { P => true, _ => false })`
//! - `assert!` 系・書式のマクロ (`format!`・`println!`・`write!` など)・`panic!` 系・`dbg!`
//!   → `!` を空白にした関数呼び出しの形 (引数は式の列。書式の名前付き引数 `name = E` は代入の
//!   形になり、`name` はどの宣言も指さない)
//! - `vec![E, ..]` / `vec![E; N]` → `vec!` を空白にした配列の式
//!
//! 入れ子の呼び出しは外側から 1 段ずつ書き換え、内側の呼び出しも写しの構文木 (token tree に
//! 隠れていた block・`use`・マクロの定義が見える) で標準と証明できたものだけを書き換える。
//! 書き換えた形が構文として読めない呼び出し (ガードの無い `matches!` の区切りが一意に
//! 決まらない、など) は書き換えず、その中の参照は従来どおり扱う。

use std::ops::Range;
use std::rc::Rc;

use tree_sitter::Tree;

use super::format_owner::{Environment, environment, environment_with_glob, has_parent_glob};
use super::*;
use crate::engine::impact::rust_std_origin::safe_attribute;

/// 入れ子のマクロを書き換える段数の上限。
const MAX_ROUNDS: usize = 4;
/// `matches!` の式とパターンの区切りとして試すカンマの数の上限。
const MAX_SPLITS: usize = 8;

/// 書き換える標準のマクロの引数の形。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Shape {
    /// 式・パターン (とガード)。
    Matches,
    /// 式の列 (書式の引数を含む)。
    Call,
    /// 配列の要素、または要素と個数。
    Array,
}

fn shape(name: &str) -> Option<Shape> {
    match name {
        "matches" => Some(Shape::Matches),
        "assert" | "assert_eq" | "assert_ne" | "debug_assert" | "debug_assert_eq"
        | "debug_assert_ne" | "format" | "format_args" | "print" | "println" | "eprint"
        | "eprintln" | "write" | "writeln" | "panic" | "unreachable" | "todo" | "unimplemented"
        | "dbg" => Some(Shape::Call),
        "vec" => Some(Shape::Array),
        _ => None,
    }
}

/// 呼び出しの名前。パスで書いた名前 (`std::assert!`) は扱わない。
fn macro_name<'s>(invocation: Node<'_>, source: &'s [u8]) -> Option<&'s str> {
    let callee = invocation.child_by_field_name("macro")?;
    if callee.kind() != "identifier" {
        return None;
    }
    let text = callee.utf8_text(source).ok()?;
    Some(text.strip_prefix("r#").unwrap_or(text))
}

/// 元のソースの区間を写しへそのまま写した区間。`original` と `copy` はどちらも昇順。
#[derive(Clone, Copy, Debug)]
struct CopiedSpan {
    original: usize,
    copy: usize,
    len: usize,
}

/// 区間 `range` を `text` に置き換える。
#[derive(Clone, Debug)]
struct Edit {
    range: Range<usize>,
    text: Vec<u8>,
}

/// `range` (`from` の座標) を含む写した区間があれば、`to` の座標の区間を返す。
fn translate(
    spans: &[CopiedSpan],
    range: Range<usize>,
    from: fn(&CopiedSpan) -> usize,
    to: fn(&CopiedSpan) -> usize,
) -> Option<Range<usize>> {
    let index = spans
        .partition_point(|span| from(span) <= range.start)
        .checked_sub(1)?;
    let span = spans[index];
    let offset = range.start - from(&span);
    (range.end <= from(&span) + span.len && offset < span.len)
        .then(|| to(&span) + offset..to(&span) + offset + range.len())
}

/// 写しの区間を元のソースの区間に戻す。`spans` が無ければ元のソースそのもの。
fn to_original(spans: Option<&[CopiedSpan]>, range: Range<usize>) -> Option<Range<usize>> {
    match spans {
        None => Some(range),
        Some(spans) => translate(spans, range, |span| span.copy, |span| span.original),
    }
}

/// 元のソースに編集を当てた写しと、写した区間。編集どうしが重なれば `None`。
fn apply(source: &[u8], edits: &[Edit]) -> Option<(Vec<u8>, Vec<CopiedSpan>)> {
    let mut order: Vec<&Edit> = edits.iter().collect();
    order.sort_by_key(|edit| edit.range.start);
    let extra: usize = edits.iter().map(|edit| edit.text.len()).sum();
    let mut text = Vec::with_capacity(source.len() + extra);
    let mut spans = Vec::new();
    let mut position = 0;
    for edit in order {
        if edit.range.start < position || edit.range.end > source.len() || edit.range.is_empty() {
            return None;
        }
        if edit.range.start > position {
            spans.push(CopiedSpan {
                original: position,
                copy: text.len(),
                len: edit.range.start - position,
            });
            text.extend_from_slice(&source[position..edit.range.start]);
        }
        text.extend_from_slice(&edit.text);
        position = edit.range.end;
    }
    if position < source.len() {
        spans.push(CopiedSpan {
            original: position,
            copy: text.len(),
            len: source.len() - position,
        });
        text.extend_from_slice(&source[position..]);
    }
    Some((text, spans))
}

/// block・インラインの module の中へ入るときの、マクロの名前の環境 (外側のスコープまでを
/// 合わせたもの)。`use super::*;` や、名前を確かめられない import・属性・マクロを含む
/// スコープの中は証明しない (`None`)。
fn enter(scope: Node<'_>, text: &[u8], outer: Option<Rc<Environment>>) -> Option<Rc<Environment>> {
    let outer = outer?;
    if has_parent_glob(scope) {
        return None;
    }
    // 書き換えの候補はどれも標準の式のマクロなので、スコープの文の位置のマクロとして数えても
    // `plain` の結果は変わらない (候補ごとに除いて数え直さない)。
    let inner = environment(scope, text, None, true)?;
    let mut combined = (*outer).clone();
    combined.merge(inner);
    Some(Rc::new(combined))
}

/// 書き換えた呼び出しだけを `let _ = …;` の初期化子として読み、思った形の式になるか。
fn valid_rewrite(invocation: Node<'_>, text: &[u8], edits: &[Edit], shape: Shape) -> bool {
    let start = invocation.start_byte();
    let local: Vec<Edit> = edits
        .iter()
        .map(|edit| Edit {
            range: edit.range.start - start..edit.range.end - start,
            text: edit.text.clone(),
        })
        .collect();
    let Some((rewritten, _)) = apply(&text[start..invocation.end_byte()], &local) else {
        return false;
    };
    const PREFIX: &[u8] = b"fn f() {\nlet _ = ";
    let mut snippet = PREFIX.to_vec();
    snippet.extend_from_slice(&rewritten);
    snippet.extend_from_slice(b"\n;\n}\n");
    let Ok(tree) = parser::parse_source(&snippet, LangId::Rust) else {
        return false;
    };
    let root = tree.root_node();
    if root.has_error() {
        return false;
    }
    // `vec!` を空白にした形は、式の前に空白が残る。
    let leading = rewritten
        .iter()
        .take_while(|byte| byte.is_ascii_whitespace())
        .count();
    let expected = PREFIX.len() + leading..PREFIX.len() + rewritten.len();
    let Some(value) = root
        .descendant_for_byte_range(expected.start, expected.end)
        .filter(|node| node.byte_range() == expected)
    else {
        return false;
    };
    if value
        .parent()
        .is_none_or(|parent| parent.kind() != "let_declaration")
    {
        return false;
    }
    match shape {
        Shape::Call => value.kind() == "call_expression",
        Shape::Array => matches!(
            value.kind(),
            "array_expression"
                | "tuple_expression"
                | "parenthesized_expression"
                | "unit_expression"
        ),
        Shape::Matches => {
            let Some(expression) = value
                .named_child(0)
                .filter(|_| {
                    value.kind() == "parenthesized_expression" && value.named_child_count() == 1
                })
                .filter(|node| node.kind() == "match_expression")
            else {
                return false;
            };
            let scrutinee_parenthesized = expression
                .child_by_field_name("value")
                .is_some_and(|node| node.kind() == "parenthesized_expression");
            let Some(body) = expression.child_by_field_name("body") else {
                return false;
            };
            let mut cursor = body.walk();
            let arms: Vec<Node<'_>> = body
                .named_children(&mut cursor)
                .filter(|node| !matches!(node.kind(), "line_comment" | "block_comment"))
                .collect();
            scrutinee_parenthesized
                && arms.len() == 2
                && arms.iter().all(|arm| arm.kind() == "match_arm")
                && arms[1]
                    .child_by_field_name("pattern")
                    .and_then(|pattern| pattern.utf8_text(&snippet).ok())
                    == Some("_")
        }
    }
}

/// 呼び出しを書き換える編集 (`text` の座標)。思った形の式として読めなければ `None`。
fn plan(invocation: Node<'_>, shape: Shape, text: &[u8]) -> Option<Vec<Edit>> {
    let callee = invocation.child_by_field_name("macro")?;
    let mut cursor = invocation.walk();
    let children: Vec<Node<'_>> = invocation.children(&mut cursor).collect();
    let [first, bang, body] = children.as_slice() else {
        return None;
    };
    if first.id() != callee.id() || bang.kind() != "!" || body.kind() != "token_tree" {
        return None;
    }
    let count = body.child_count();
    let open = body.child(0)?;
    let close = body.child(count.checked_sub(1)?)?;
    let parenthesized = open.kind() == "(" && close.kind() == ")";
    match shape {
        Shape::Call => {
            if !parenthesized {
                return None;
            }
            let edits = vec![Edit {
                range: bang.byte_range(),
                text: b" ".to_vec(),
            }];
            valid_rewrite(invocation, text, &edits, shape).then_some(edits)
        }
        Shape::Array => {
            if !parenthesized && !(open.kind() == "[" && close.kind() == "]") {
                return None;
            }
            let range = callee.start_byte()..bang.end_byte();
            let edits = vec![Edit {
                text: vec![b' '; range.len()],
                range,
            }];
            valid_rewrite(invocation, text, &edits, shape).then_some(edits)
        }
        Shape::Matches => {
            if !parenthesized || count < 2 {
                return None;
            }
            let inner: Vec<Node<'_>> = (1..count - 1)
                .filter_map(|index| body.child(index))
                .collect();
            let significant: Vec<usize> = (0..inner.len())
                .filter(|&index| !matches!(inner[index].kind(), "line_comment" | "block_comment"))
                .collect();
            // 末尾のカンマはパターンに含めない。
            let (last, end) = match significant.last() {
                Some(&index) if inner[index].kind() == "," => (index, inner[index].start_byte()),
                _ => (inner.len(), close.start_byte()),
            };
            let first_significant = *significant.first()?;
            let mut found = None;
            let mut tried = 0;
            for (index, comma) in inner.iter().enumerate().take(last) {
                if comma.kind() != "," || index <= first_significant {
                    continue;
                }
                // パターンが空になる区切りは試さない。
                if !significant
                    .iter()
                    .any(|&other| other > index && other < last)
                {
                    continue;
                }
                tried += 1;
                if tried > MAX_SPLITS {
                    return None;
                }
                let edits = vec![
                    Edit {
                        range: callee.start_byte()..open.end_byte(),
                        text: b"(match (".to_vec(),
                    },
                    Edit {
                        range: comma.byte_range(),
                        text: b") {".to_vec(),
                    },
                    Edit {
                        range: end..close.end_byte(),
                        text: b" => true, _ => false })".to_vec(),
                    },
                ];
                if valid_rewrite(invocation, text, &edits, shape) && found.replace(edits).is_some()
                {
                    // 式とパターンの区切りが一意に決まらない。
                    return None;
                }
            }
            found
        }
    }
}

/// 書き換えた呼び出しの編集と、呼び出しの名前の区間 (どちらも元のソースの座標)。
struct Rewrite {
    edits: Vec<Edit>,
    callee: Range<usize>,
}

/// 標準のマクロの写し。
pub(in crate::engine::impact) struct MacroCopy {
    source: Vec<u8>,
    tree: Tree,
    index: RustPathOwnerIndex,
    spans: Vec<CopiedSpan>,
    /// 書き換えた呼び出しの名前 (元のソースの座標、昇順で重ならない)。名前の参照は写しに
    /// 対応させない。
    callees: Vec<Range<usize>>,
    /// 写しの import・glob・経路の名前 (`import_facts::rust_ref_facts_of_tree`)。マクロの引数の
    /// 中の `use` は元の構文木では token tree に隠れるので、写しから集める。
    evidence: std::cell::OnceCell<(HashSet<String>, bool)>,
    /// 名前の束縛を確かめられないスコープと項目の id ([`uncertain_scopes`])。この中の参照は写しに
    /// 対応させない。
    uncertain: HashSet<usize>,
}

impl MacroCopy {
    /// `node` (元の構文木の識別子) が、書き換えの候補のマクロの引数の中にあるか。写しを
    /// 作る前の安価な判定で、構文木に現れる一番外側の呼び出しの名前だけを見る。
    pub(in crate::engine::impact) fn candidate(node: Node<'_>, source: &[u8]) -> bool {
        let mut ancestor = node.parent();
        let mut in_token_tree = false;
        while let Some(current) = ancestor {
            match current.kind() {
                "token_tree" => in_token_tree = true,
                "macro_invocation" => {
                    return in_token_tree
                        && macro_name(current, source).is_some_and(|name| shape(name).is_some());
                }
                _ => return false,
            }
            ancestor = current.parent();
        }
        false
    }

    /// ファイルの写しを作る。標準と証明できて書き換えた呼び出しが無ければ `None`。
    /// `original` は元の構文木の索引で、標準のマクロの証明 (祖先のファイルの環境の
    /// キャッシュ) だけに使う。
    pub(in crate::engine::impact) fn build(
        node: Node<'_>,
        source: &[u8],
        original: &RustPathOwnerIndex,
        dir: &Path,
        ref_path: &str,
        project: &RustPathOwnerProjectProof,
    ) -> Option<Self> {
        let mut root = node;
        while let Some(parent) = root.parent() {
            root = parent;
        }
        if root.kind() != "source_file" || root.has_error() {
            return None;
        }
        let base = Rc::new(
            original
                .standard_macro_base(dir, ref_path, project)?
                .clone(),
        );
        let mut edits: Vec<Edit> = Vec::new();
        let mut callees: Vec<Range<usize>> = Vec::new();
        let mut considered = HashSet::new();
        let mut current: Option<(Vec<u8>, Tree, Vec<CopiedSpan>)> = None;
        for _ in 0..MAX_ROUNDS {
            let (text, tree_root, spans) = match &current {
                Some((text, tree, spans)) => {
                    (text.as_slice(), tree.root_node(), Some(spans.as_slice()))
                }
                None => (source, root, None),
            };
            let mut found = Vec::new();
            // (ノード, 親の種類, 外側のスコープまでのマクロの名前の環境)。スコープの環境は
            // 入るときに 1 度だけ作り、呼び出しごとに祖先をたどり直さない。
            let mut pending = vec![(tree_root, "", Some(base.clone()))];
            while let Some((node, parent_kind, scope)) = pending.pop() {
                if node.kind() == "macro_invocation" {
                    // item の位置 (ファイル・module などの直下) の呼び出しは書き換えない。
                    if !matches!(parent_kind, "source_file" | "declaration_list")
                        && let Some(scope) = &scope
                    {
                        found.extend(rewrite(node, text, spans, &mut considered, scope, project));
                    }
                    continue;
                }
                let scope = if node.kind() == "block"
                    || (node.kind() == "declaration_list" && parent_kind == "mod_item")
                {
                    enter(node, text, scope)
                } else {
                    scope
                };
                let mut cursor = node.walk();
                pending.extend(
                    node.named_children(&mut cursor)
                        .map(|child| (child, node.kind(), scope.clone())),
                );
            }
            if found.is_empty() {
                break;
            }
            let mut next = edits.clone();
            next.extend(
                found
                    .iter()
                    .flat_map(|rewrite| rewrite.edits.iter().cloned()),
            );
            let Some((copy, copy_spans)) = apply(source, &next) else {
                break;
            };
            let Ok(tree) = parser::parse_source(&copy, LangId::Rust) else {
                break;
            };
            if tree.root_node().has_error() {
                break;
            }
            edits = next;
            callees.extend(found.into_iter().map(|rewrite| rewrite.callee));
            current = Some((copy, tree, copy_spans));
        }
        let (source, tree, spans) = current?;
        let index = RustPathOwnerIndex::build(tree.root_node(), &source);
        let uncertain = uncertain_scopes(tree.root_node(), &source, &base);
        callees.sort_by_key(|callee| callee.start);
        Some(Self {
            source,
            tree,
            index,
            spans,
            callees,
            evidence: std::cell::OnceCell::new(),
            uncertain,
        })
    }

    /// 元の構文木の識別子 `node` に当たる写しのノードと、それが `use` の宣言の中か。`node` を
    /// 囲むマクロの呼び出しがすべて書き換えられ、囲むスコープの名前の束縛を確かめられる
    /// ([`uncertain_scopes`]) ときだけ返す。
    pub(in crate::engine::impact) fn site<'c>(
        &'c self,
        node: Node<'_>,
    ) -> Option<(Node<'c>, bool)> {
        let range = node.byte_range();
        // 名前の区間は昇順で重ならないので、`range` の終わりより前に始まる最後の 1 つだけを見る。
        let after = self
            .callees
            .partition_point(|callee| callee.start < range.end);
        if after
            .checked_sub(1)
            .is_some_and(|last| range.start < self.callees[last].end)
        {
            return None;
        }
        let copy = translate(&self.spans, range, |span| span.original, |span| span.copy)?;
        // 根から 1 度だけ下って、写しの識別子と、それを囲む token tree (書き換えていない
        // マクロ) の有無を確かめる。`Node::parent` は呼ぶたびに根から探し直すので使わない。
        let mut cursor = self.tree.walk();
        let mut in_use = false;
        loop {
            let current = cursor.node();
            if current.kind() == "token_tree" || self.uncertain.contains(&current.id()) {
                return None;
            }
            in_use |= current.kind() == "use_declaration";
            if current.byte_range() == copy
                && matches!(
                    current.kind(),
                    "identifier"
                        | "type_identifier"
                        | "field_identifier"
                        | "shorthand_field_identifier"
                )
            {
                return Some((current, in_use));
            }
            cursor.goto_first_child_for_byte(copy.start)?;
            let child = cursor.node();
            if child.start_byte() > copy.start || child.end_byte() < copy.end {
                return None;
            }
        }
    }

    /// 写しに `name` を名指す import・glob・経路があるか (振り分けの証拠。元のファイルの
    /// 証拠に足す)。
    pub(in crate::engine::impact) fn has_symbol_evidence(&self, name: &str) -> bool {
        let (names, glob) = self.evidence.get_or_init(|| {
            super::super::import_facts::rust_ref_facts_of_tree(self.tree.root_node(), &self.source)
        });
        *glob || names.contains(name)
    }

    pub(in crate::engine::impact) fn source(&self) -> &[u8] {
        &self.source
    }

    pub(in crate::engine::impact) fn index(&self) -> &RustPathOwnerIndex {
        &self.index
    }
}

/// 写しの中で、名前の束縛を確かめられないスコープと項目の id。標準のマクロの同定と同じく、
/// 参照ファイルと祖先のファイルの環境 `base` に、外側の block・インラインの module の環境を重ねて
/// 調べる。
///
/// - block・インラインの module の中身・ファイルで、マクロの名前の環境を作れない
///   ([`environment_with_glob`] が `None`。`extern crate`・安全と確かめられない属性・標準の式マクロ
///   以外の文のマクロ)、import した名前が属性・derive・文の位置のマクロの名前と重なる
///   ([`Environment::consistent`]。自作のものかもしれない)、または書き換えずに残った文の位置の
///   マクロの呼び出しがある。属性のマクロや文の位置のマクロの展開は、そのスコープ全体に効く `use`
///   や宣言を足し得る (呼び出しの前後を問わない)
/// - そのほかの位置 (impl の中など) で、安全と確かめられない属性や、import で差し替え得る名前の
///   属性の付いた項目 (属性のマクロは本体を書き換え得る)
///
/// 証明できないマクロや属性が足す束縛は元のファイルにも写しにも現れないので、その中の参照は
/// 写しに対応させず、従来どおり扱う。写しに現れる `use` (glob を含む) は振り分けの証拠
/// ([`MacroCopy::has_symbol_evidence`]) に入るので、ここでは数えない。
fn uncertain_scopes(root: Node<'_>, text: &[u8], base: &Rc<Environment>) -> HashSet<usize> {
    let mut found = HashSet::new();
    // (ノード, 親の種類, 外側のスコープまでの環境。確かめられないスコープの中では None)
    let mut pending = vec![(root, "", Some(base.clone()))];
    while let Some((node, parent_kind, outer)) = pending.pop() {
        if node.kind() == "macro_invocation" {
            continue;
        }
        let scope = matches!(node.kind(), "block" | "source_file")
            || (node.kind() == "declaration_list" && parent_kind == "mod_item");
        let environment = if scope {
            let combined = outer.and_then(|outer| {
                let (inner, _) = environment_with_glob(node, text, None, true)?;
                let mut combined = (*outer).clone();
                combined.merge(inner);
                Some(Rc::new(combined))
            });
            if combined.as_ref().is_none_or(|env| !env.consistent()) || statement_macro(node) {
                found.insert(node.id());
            }
            combined
        } else {
            outer
        };
        let attribute_unsafe =
            |attribute: Node<'_>| match (safe_attribute(attribute, text), &environment) {
                (Some(names), Some(env)) => env.shadows_any(&names),
                _ => true,
            };
        let mut cursor = node.walk();
        let mut unsafe_attribute = false;
        for child in node.named_children(&mut cursor) {
            match child.kind() {
                "attribute_item" => {
                    unsafe_attribute |= attribute_unsafe(child);
                    continue;
                }
                "inner_attribute_item" => {
                    if attribute_unsafe(child) {
                        found.insert(node.id());
                    }
                    continue;
                }
                "line_comment" | "block_comment" => continue,
                _ => {}
            }
            if std::mem::take(&mut unsafe_attribute) {
                found.insert(child.id());
            }
            pending.push((child, node.kind(), environment.clone()));
        }
    }
    found
}

/// スコープの直下 (文の位置) に、書き換えずに残ったマクロの呼び出しがあるか。
fn statement_macro(scope: Node<'_>) -> bool {
    let mut cursor = scope.walk();
    scope.named_children(&mut cursor).any(|child| {
        child.kind() == "macro_invocation"
            || (child.kind() == "expression_statement" && {
                let mut cursor = child.walk();
                child
                    .named_children(&mut cursor)
                    .any(|node| node.kind() == "macro_invocation")
            })
    })
}

/// `invocation` が標準のマクロと証明でき、普通の式に書き換えられれば、その編集。`scope` は
/// 呼び出しを囲むスコープまでのマクロの名前の環境 ([`enter`])。
fn rewrite(
    invocation: Node<'_>,
    text: &[u8],
    spans: Option<&[CopiedSpan]>,
    considered: &mut HashSet<usize>,
    scope: &Environment,
    project: &RustPathOwnerProjectProof,
) -> Option<Rewrite> {
    let name = macro_name(invocation, text)?;
    let shape = shape(name)?;
    let range = to_original(spans, invocation.byte_range())?;
    if !considered.insert(range.start) {
        return None;
    }
    // `#[macro_export]` は crate root の名前を差し替えるので、同じ名前の `macro_rules!` が
    // プロジェクトのどこかにあれば標準とみなさない。
    if project.may_define_macro(name) || !scope.plain(name) {
        return None;
    }
    let edits = plan(invocation, shape, text)?
        .into_iter()
        .map(|edit| {
            Some(Edit {
                range: to_original(spans, edit.range)?,
                text: edit.text,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    let callee = to_original(spans, invocation.child_by_field_name("macro")?.byte_range())?;
    Some(Rewrite { edits, callee })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn copy_text(source: &str) -> Option<String> {
        let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
        let mut edits = Vec::new();
        let mut pending = vec![tree.root_node()];
        while let Some(node) = pending.pop() {
            if node.kind() == "macro_invocation" {
                let name = macro_name(node, source.as_bytes()).unwrap();
                if let Some(found) =
                    shape(name).and_then(|shape| plan(node, shape, source.as_bytes()))
                {
                    edits.extend(found);
                }
                continue;
            }
            let mut cursor = node.walk();
            pending.extend(node.named_children(&mut cursor));
        }
        if edits.is_empty() {
            return None;
        }
        let (text, _) = apply(source.as_bytes(), &edits).unwrap();
        Some(String::from_utf8(text).unwrap())
    }

    #[test]
    fn rewrites_standard_macro_shapes() {
        assert_eq!(
            copy_text("fn f() { matches!(kind, RefKind::Definition) }").as_deref(),
            Some("fn f() { (match (kind) { RefKind::Definition => true, _ => false }) }")
        );
        // ガード、or パターン、末尾のカンマ、複数行
        assert_eq!(
            copy_text("fn f() { matches!(\n    kind,\n    A::B | A::C if ok,\n) }").as_deref(),
            Some("fn f() { (match (\n    kind) {\n    A::B | A::C if ok => true, _ => false }) }")
        );
        // 式の中のカンマ (型引数・クロージャ) で区切らない
        assert_eq!(
            copy_text("fn f() { matches!(g::<A, B>(|x, y| x), Some(_)) }").as_deref(),
            Some("fn f() { (match (g::<A, B>(|x, y| x)) { Some(_) => true, _ => false }) }")
        );
        assert_eq!(
            copy_text("fn f() { assert_eq!(a, b, \"{}\", c); }").as_deref(),
            Some("fn f() { assert_eq (a, b, \"{}\", c); }")
        );
        assert_eq!(
            copy_text("fn f() { let v = vec![A::B; n]; }").as_deref(),
            Some("fn f() { let v =     [A::B; n]; }")
        );
        assert_eq!(
            copy_text("fn f() { format!(\"{x}\", x = y); }").as_deref(),
            Some("fn f() { format (\"{x}\", x = y); }")
        );
    }

    #[test]
    fn keeps_unreadable_or_ambiguous_calls() {
        // 区切りのカンマが無い・パターンが空
        assert_eq!(copy_text("fn f() { matches!(kind) }"), None);
        assert_eq!(copy_text("fn f() { matches!(kind,) }"), None);
        // 式の位置に書けない区切り方 (`{` の書き換えた形が式として読めない)
        assert_eq!(copy_text("fn f() { assert!{ x } }"), None);
        assert_eq!(copy_text("fn f() { vec!{ x } }"), None);
        // 標準でない形の引数
        assert_eq!(copy_text("fn f() { vec![x; ] }"), None);
    }

    #[test]
    fn translate_maps_only_ranges_inside_one_copied_span() {
        let source = b"fn f() { matches!(a, b) }";
        let edits = vec![
            Edit {
                range: 9..18,
                text: b"(match (".to_vec(),
            },
            Edit {
                range: 19..20,
                text: b") {".to_vec(),
            },
            Edit {
                range: 22..23,
                text: b" => true, _ => false })".to_vec(),
            },
        ];
        let (text, spans) = apply(source, &edits).unwrap();
        assert_eq!(
            String::from_utf8(text.clone()).unwrap(),
            "fn f() { (match (a) { b => true, _ => false }) }"
        );
        let a = translate(&spans, 18..19, |span| span.original, |span| span.copy).unwrap();
        assert_eq!(&text[a], b"a");
        let b = translate(&spans, 21..22, |span| span.original, |span| span.copy).unwrap();
        assert_eq!(&text[b.clone()], b"b");
        assert_eq!(
            translate(&spans, b, |span| span.copy, |span| span.original),
            Some(21..22)
        );
        // 書き換えた区間をまたぐ範囲は写さない
        assert_eq!(
            translate(&spans, 17..19, |span| span.original, |span| span.copy),
            None
        );
    }
}
