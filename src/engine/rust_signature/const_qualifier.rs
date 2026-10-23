//! Rust の関数に `const` を足しただけかを比べる、関数自身の `const` を除いた宣言のトークン。
//!
//! const の文脈の外からの呼び出しは `const` の有無で変わらない (Rust Reference の
//! Const functions) ので、足す変更は呼び出し側に修正を求めない。外す変更は const の文脈の
//! 呼び出しを壊しうるので証明しない。比べるのは呼び出し契約 (正規化したヘッダと、その意味を
//! 決める同じファイルの宣言) だけで、本体が const fn として成り立つか (const にできない処理・
//! 必要な Rust の版) は rustc に任せる。

use std::collections::{BTreeSet, HashMap};

use super::parameter_patterns::{append_header, append_inner_attributes, append_outer_attributes};
use super::{Children, ControlFlow, HashSet, Node, Walk, any_node, walk_preorder};

/// 関数の `const` の有無と、それを除いた宣言 (ヘッダ・属性・外側の宣言) のトークン列。
#[derive(Debug)]
pub(crate) struct RustConstQualifier {
    is_const: bool,
    tokens: Vec<String>,
    /// ヘッダと、impl Trait のときは本体に現れる識別子 (関数自身の名前の宣言を除く)。
    names: BTreeSet<String>,
    /// 関数自身の開始位置。同じファイルの宣言の比較から関数自身を除く。
    start_byte: usize,
}

impl RustConstQualifier {
    /// ヘッダ・属性・外側の宣言の違いが、`old` に `const` を足したことだけか。
    /// 宣言に現れる名前の、同じファイルでの宣言は見ない。
    pub(crate) fn header_only_adds_const_to(&self, old: &Self) -> bool {
        self.is_const && !old.is_const && self.tokens == old.tokens
    }

    /// `old` (旧ファイルの宣言 `old_items`) からこの宣言 (新ファイルの宣言 `items`) への
    /// 変更が、`const` を足しただけか。ヘッダの字面が同じでも、同じファイルの `type Arg = u8`
    /// を `u16` に変えれば `fn f(_: Arg)` の呼び出しは壊れるので、宣言に現れる名前の宣言と、
    /// どの名前に効くか分からない宣言も旧新で同じことを求める。名前の宣言に現れる名前の
    /// 宣言 (型別名の連鎖・`use` の別名・定数の式が使う定数) もたどって比べる。
    pub(crate) fn only_adds_const_to(
        &self,
        items: &RustFileItems,
        old: &Self,
        old_items: &RustFileItems,
    ) -> bool {
        if !self.header_only_adds_const_to(old)
            || items.non_ascii_names
            || old_items.non_ascii_names
            || items.opaque != old_items.opaque
            || items.macro_positions != old_items.macro_positions
        {
            return false;
        }
        let mut pending: Vec<&str> = self
            .names
            .iter()
            .map(String::as_str)
            .chain(items.opaque_names.iter().map(String::as_str))
            .collect();
        if pending.iter().any(|name| !name.is_ascii()) {
            return false;
        }
        pending.sort_unstable();
        pending.dedup();
        let mut seen: HashSet<&str> = pending.iter().copied().collect();
        while let Some(name) = pending.pop() {
            let declarations = items.declarations(name, self.start_byte);
            if declarations != old_items.declarations(name, old.start_byte)
                && !items.dependency_only_adds_const(
                    name,
                    self.start_byte,
                    old_items,
                    old.start_byte,
                )
            {
                return false;
            }
            for token in declarations.into_iter().flatten() {
                if !is_name_token(token) {
                    continue;
                }
                // NFC で同じになる別の綴りを引き当てられないので、非 ASCII の名前は証明しない。
                if !token.is_ascii() {
                    return false;
                }
                if seen.insert(name_key(token)) {
                    pending.push(name_key(token));
                }
            }
        }
        true
    }
}

/// 識別子になりうるトークン (キーワードも含む。宣言が無ければ比べても結果は変わらない)。
fn is_name_token(token: &str) -> bool {
    token
        .chars()
        .next()
        .is_some_and(|first| first == '_' || first.is_alphabetic())
}

/// 宣言を引く名前。raw identifier (`r#Arg`) は `Arg` と同じ名前。
fn name_key(token: &str) -> &str {
    token.strip_prefix("r#").unwrap_or(token)
}

/// 同じファイルの名前付きの宣言 (関数の本体の中を除く) の、名前ごとのトークン列。
#[derive(Debug)]
pub(crate) struct RustFileItems {
    /// 名前 → (宣言の開始位置, 宣言を囲むモジュール・impl などの属性とヘッダと、外側の属性を
    /// 含む宣言のトークン列)。モジュールは本体を除く。囲む宣言を含めるので、別のモジュールとの
    /// 宣言の入れ替えも違いになる。`use` は現れる名前すべてで引く (別名の束縛先をたどる)。
    by_name: HashMap<String, Vec<(usize, Vec<String>)>>,
    /// 関数の開始位置 → (const の証拠, by_name のトークン列内で本体が始まる位置)。
    dependency_functions: HashMap<usize, (RustConstQualifier, usize)>,
    /// impl のヘッダと、どの名前に効くか分からないマクロ・入れ子の宣言。
    /// 関数の本体などの中の impl・マクロは、外側の宣言ごと比べる。
    opaque: Vec<Vec<String>>,
    /// opaque 内の参照名。囲む関数の宣言名だけは AST 上の位置で除く。
    opaque_names: BTreeSet<String>,
    /// line!/column! や展開を証明できないマクロの、行と Unicode 文字の列。
    macro_positions: Vec<(usize, usize)>,
    /// 非 ASCII の名前の宣言がある。NFC で同じになる綴りを区別できないので証明しない。
    non_ascii_names: bool,
}

impl RustFileItems {
    /// ファイルの宣言を集める。解析エラーがあれば `None`。
    pub(crate) fn new(root: Node<'_>, source: &[u8]) -> Option<Self> {
        if root.has_error() {
            return None;
        }
        let mut items = Self {
            by_name: HashMap::new(),
            dependency_functions: HashMap::new(),
            opaque: Vec::new(),
            opaque_names: BTreeSet::new(),
            macro_positions: macro_positions(root, source)?,
            non_ascii_names: false,
        };
        let mut containers = vec![(root, Vec::new())];
        while let Some((container, mut scope)) = containers.pop() {
            let mut attributes = Vec::new();
            let mut cursor = container.walk();
            for child in container.named_children(&mut cursor) {
                if child.is_extra() {
                    continue;
                }
                match child.kind() {
                    "attribute_item" => {
                        append_item_tokens(child, source, &mut attributes)?;
                        continue;
                    }
                    // 本体の先頭の `#![..]` は、後に続く宣言すべてに効く。
                    "inner_attribute_item" => {
                        append_item_tokens(child, source, &mut scope)?;
                        continue;
                    }
                    _ => {}
                }
                let mut tokens = scope.clone();
                tokens.push("<item>".to_string());
                tokens.append(&mut attributes);
                // ファイル直下の `m!(..);` は expression_statement に包まれる。マクロの定義が
                // 変わると、同じ呼び出しが別の宣言を作りうるので定義も比べる。
                if matches!(
                    child.kind(),
                    "macro_invocation" | "expression_statement" | "macro_definition"
                ) {
                    items.collect_opaque_names(child, source, &tokens, &[])?;
                    append_item_tokens(child, source, &mut tokens)?;
                    items.opaque.push(tokens);
                    continue;
                }
                if matches!(child.kind(), "use_declaration" | "extern_crate_declaration") {
                    let mut declaration = Vec::new();
                    append_item_tokens(child, source, &mut declaration)?;
                    let names: BTreeSet<String> = declaration
                        .iter()
                        .filter(|token| is_name_token(token))
                        .cloned()
                        .collect();
                    tokens.append(&mut declaration);
                    for name in names {
                        items.insert(&name, child.start_byte(), tokens.clone());
                    }
                    continue;
                }
                let container_kind = matches!(
                    child.kind(),
                    "mod_item" | "trait_item" | "impl_item" | "foreign_mod_item"
                );
                if child.kind() == "impl_item" {
                    // 空の trait impl も auto trait などを変えるので、ヘッダ自体を残す。
                    let mut declaration = tokens.clone();
                    let skipped: Vec<_> = child
                        .child_by_field_name("body")
                        .map(|body| body.id())
                        .into_iter()
                        .collect();
                    items.collect_opaque_names(child, source, &tokens, &skipped)?;
                    append_header(child, source, &HashSet::new(), None, &mut declaration)?;
                    items.opaque.push(declaration);
                }
                if let Some(name) = child.child_by_field_name("name") {
                    let name = name.utf8_text(source).ok()?;
                    let mut declaration = tokens.clone();
                    if child.kind() == "mod_item" {
                        // 本体の宣言は個別に集める (対象の関数自身を含みうる)。
                        append_header(child, source, &HashSet::new(), None, &mut declaration)?;
                    } else if child.kind() == "function_item"
                        && let Some(qualifier) = rust_const_qualifier(child, source)
                    {
                        let body = child.child_by_field_name("body")?;
                        append_item_tokens_omitting(
                            child,
                            source,
                            Some(body.id()),
                            &mut declaration,
                        )?;
                        let body_start = declaration.len();
                        append_item_tokens(body, source, &mut declaration)?;
                        items
                            .dependency_functions
                            .insert(child.start_byte(), (qualifier, body_start));
                    } else {
                        append_item_tokens(child, source, &mut declaration)?;
                    }
                    items.insert(name, child.start_byte(), declaration);
                }
                if container_kind && let Some(body) = child.child_by_field_name("body") {
                    // 囲む宣言の属性 (`#[cfg(..)] impl ..`) も中の宣言の一部として比べる。
                    let mut inner = tokens;
                    inner.push("<scope>".to_string());
                    append_header(child, source, &HashSet::new(), None, &mut inner)?;
                    containers.push((body, inner));
                } else {
                    // 関数の本体や定数の初期化式の中の impl は、その外の型にも効く。
                    items.collect_nested(child, source, tokens)?;
                }
            }
        }
        Some(items)
    }

    fn insert(&mut self, name: &str, start: usize, tokens: Vec<String>) {
        self.non_ascii_names |= !name.is_ascii();
        self.by_name
            .entry(name_key(name).to_string())
            .or_default()
            .push((start, tokens));
    }

    /// 入れ子の impl などは、外側の宣言の cfg でも有効性が変わるので宣言全体を比べる。
    fn collect_nested(
        &mut self,
        node: Node<'_>,
        source: &[u8],
        mut tokens: Vec<String>,
    ) -> Option<()> {
        if any_node(node, Children::Named, |node| {
            matches!(
                node.kind(),
                "impl_item" | "macro_definition" | "macro_invocation"
            )
        }) {
            let skipped: Vec<_> = (node.kind() == "function_item")
                .then(|| node.child_by_field_name("name").map(|name| name.id()))
                .flatten()
                .into_iter()
                .collect();
            self.collect_opaque_names(node, source, &tokens, &skipped)?;
            tokens.push("<nested>".to_string());
            // 対象関数自身に assert! などがあっても、const 追加だけなら同じ宣言とする。
            let mut omitted = None;
            if node.kind() == "function_item" {
                let mut cursor = node.walk();
                if let Some(modifiers) = node
                    .named_children(&mut cursor)
                    .find(|child| child.kind() == "function_modifiers")
                {
                    let mut cursor = modifiers.walk();
                    omitted = modifiers
                        .children(&mut cursor)
                        .find(|child| child.kind() == "const")
                        .map(|keyword| keyword.id());
                }
            }
            let mut header = Vec::new();
            if node.kind() == "function_item"
                && append_header(node, source, &HashSet::new(), omitted, &mut header).is_some()
            {
                tokens.extend(header);
                tokens.push("<body>".to_string());
                append_item_tokens(node.child_by_field_name("body")?, source, &mut tokens)?;
            } else {
                append_item_tokens_omitting(node, source, omitted, &mut tokens)?;
            }
            self.opaque.push(tokens);
        }
        Some(())
    }

    fn collect_opaque_names(
        &mut self,
        node: Node<'_>,
        source: &[u8],
        prefix: &[String],
        skipped: &[usize],
    ) -> Option<()> {
        self.opaque_names.extend(
            prefix
                .iter()
                .filter(|token| is_name_token(token))
                .map(|token| name_key(token).to_owned()),
        );
        collect_names(node, source, skipped, &mut self.opaque_names)
    }

    /// `name` の宣言のトークン列 (`excluded` に始まる宣言を除き、順序をそろえたもの)。
    fn declarations(&self, name: &str, excluded: usize) -> Vec<&[String]> {
        let mut declarations: Vec<&[String]> = self
            .by_name
            .get(name)
            .into_iter()
            .flatten()
            .filter(|(start, _)| *start != excluded)
            .map(|(_, tokens)| tokens.as_slice())
            .collect();
        declarations.sort_unstable();
        declarations
    }

    /// 依存先が一意な関数で、本体も維持した const 追加だけなら同じ宣言として扱う。
    fn dependency_only_adds_const(
        &self,
        name: &str,
        excluded: usize,
        old: &Self,
        old_excluded: usize,
    ) -> bool {
        let Some((start, tokens)) = self.unique_declaration(name, excluded) else {
            return false;
        };
        let Some((old_start, old_tokens)) = old.unique_declaration(name, old_excluded) else {
            return false;
        };
        let Some((qualifier, body_start)) = self.dependency_functions.get(start) else {
            return false;
        };
        let Some((old_qualifier, old_body_start)) = old.dependency_functions.get(old_start) else {
            return false;
        };
        qualifier.header_only_adds_const_to(old_qualifier)
            && tokens[*body_start..] == old_tokens[*old_body_start..]
    }

    fn unique_declaration(&self, name: &str, excluded: usize) -> Option<&(usize, Vec<String>)> {
        let mut declarations = self
            .by_name
            .get(name)?
            .iter()
            .filter(|(start, _)| *start != excluded);
        let declaration = declarations.next()?;
        declarations.next().is_none().then_some(declaration)
    }
}

/// 展開位置を変えてよいのは、入れ子のマクロ・属性・呼び出しがないと分かる場合だけ。
fn macro_positions(root: Node<'_>, source: &[u8]) -> Option<Vec<(usize, usize)>> {
    let mut definitions: HashMap<&str, Vec<Node<'_>>> = HashMap::new();
    let mut imported = BTreeSet::new();
    let mut uncertain_bindings = false;
    let mut invocations = Vec::new();
    let flow = walk_preorder(root, Children::Named, |node, _| {
        match node.kind() {
            "macro_definition" => {
                let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|name| name.utf8_text(source).ok())
                else {
                    return ControlFlow::Break(());
                };
                definitions.entry(name_key(name)).or_default().push(node);
            }
            "use_declaration" => {
                if collect_names(node, source, &[], &mut imported).is_none() {
                    return ControlFlow::Break(());
                }
                uncertain_bindings |= any_node(node, Children::All, |child| child.kind() == "*");
            }
            "attribute_item" => {
                let mut next = node.next_named_sibling();
                while next.is_some_and(|next| next.is_extra() || next.kind() == "attribute_item") {
                    next = next.and_then(|next| next.next_named_sibling());
                }
                uncertain_bindings |= next.is_none_or(|declaration| {
                    append_outer_attributes(declaration, source, &mut Vec::new()).is_none()
                });
            }
            "inner_attribute_item" => {
                uncertain_bindings |= node.parent().is_none_or(|container| {
                    append_inner_attributes(container, source, &mut Vec::new()).is_none()
                });
            }
            "extern_crate_declaration" => uncertain_bindings = true,
            "macro_invocation" => {
                // item マクロが別のマクロを生成する可能性は展開せず保留する。
                let mut parent = node.parent();
                while parent.is_some_and(|parent| parent.kind() != "function_item") {
                    parent = parent.and_then(|parent| parent.parent());
                }
                uncertain_bindings |= parent.is_none();
                invocations.push(node);
            }
            _ => {}
        }
        ControlFlow::Continue(Walk::Descend)
    });
    if flow.is_break() {
        return None;
    }
    let mut starts = Vec::new();
    for invocation in invocations {
        let name = invocation.child_by_field_name("macro")?;
        let simple_name = (name.kind() == "identifier")
            .then(|| name.utf8_text(source).ok().map(name_key))
            .flatten();
        let mut cursor = invocation.walk();
        let arguments = invocation
            .named_children(&mut cursor)
            .find(|child| child.kind() == "token_tree")?;
        let position_free = simple_name.is_some_and(|name| {
            if uncertain_bindings || imported.contains(name) || !position_free_tokens(arguments) {
                return false;
            }
            if let Some(definitions) = definitions.get(name) {
                let [definition] = definitions.as_slice() else {
                    return false;
                };
                // 別スコープや呼び出しより後の定義を、同じ名前だけで解決しない。
                if definition.parent().map(|parent| parent.id()) != Some(root.id())
                    || definition.start_byte() >= invocation.start_byte()
                {
                    return false;
                }
                let mut cursor = definition.walk();
                return definition
                    .named_children(&mut cursor)
                    .filter(|child| child.kind() == "macro_rule")
                    .all(|rule| {
                        rule.child_by_field_name("right")
                            .is_some_and(position_free_tokens)
                    });
            }
            matches!(
                name,
                "assert"
                    | "assert_eq"
                    | "assert_ne"
                    | "debug_assert"
                    | "debug_assert_eq"
                    | "debug_assert_ne"
            )
        });
        if !position_free {
            starts.push(invocation.start_byte());
        }
    }
    starts.sort_unstable();
    let mut starts = starts.into_iter().peekable();
    let mut positions = Vec::new();
    let (mut line, mut column) = (0, 0);
    // tree-sitter の列は byte 数なので、column! と同じ Unicode 文字数を数える。
    for (byte, character) in std::str::from_utf8(source).ok()?.char_indices() {
        if starts.peek() == Some(&byte) {
            positions.push((line, column));
            starts.next();
        }
        if character == '\n' {
            line += 1;
            column = 0;
        } else {
            column += 1;
        }
    }
    starts.next().is_none().then_some(positions)
}

fn position_free_tokens(tree: Node<'_>) -> bool {
    !any_node(tree, Children::All, |node| {
        matches!(node.kind(), "!" | "#")
            || (node.kind() == "(" && node.parent().map(|parent| parent.id()) != Some(tree.id()))
    })
}

/// マクロの token tree は stringify! などで空白も値になるため、原文のまま比べる。
fn append_item_tokens(node: Node<'_>, source: &[u8], tokens: &mut Vec<String>) -> Option<()> {
    append_item_tokens_omitting(node, source, None, tokens)
}

fn append_item_tokens_omitting(
    node: Node<'_>,
    source: &[u8],
    omitted: Option<usize>,
    tokens: &mut Vec<String>,
) -> Option<()> {
    let flow = walk_preorder(node, Children::All, |node, _| {
        if node.is_error() || node.is_missing() {
            return ControlFlow::Break(());
        }
        if node.is_extra() || Some(node.id()) == omitted {
            return ControlFlow::Continue(Walk::Skip);
        }
        if matches!(node.kind(), "token_tree" | "token_tree_pattern") {
            let Ok(text) = node.utf8_text(source) else {
                return ControlFlow::Break(());
            };
            tokens.push(text.to_owned());
            // 原文を保持したまま、引数が使う定数などの宣言も引けるよう名前を残す。
            let mut names = BTreeSet::new();
            if collect_names(node, source, &[], &mut names).is_none() {
                return ControlFlow::Break(());
            }
            tokens.extend(names);
            return ControlFlow::Continue(Walk::Skip);
        }
        if node.child_count() == 0 {
            let Ok(text) = node.utf8_text(source) else {
                return ControlFlow::Break(());
            };
            tokens.push(text.to_owned());
        }
        ControlFlow::Continue(Walk::Descend)
    });
    flow.is_continue().then_some(())
}

/// 関数の `const` を除いた宣言のトークン列を返す。
///
/// ヘッダはコメント・空白・束縛側の `mut`・リストの末尾カンマを除いて比べ (API の
/// シグネチャと同じ正規化)、引数のパターンは比べる。外側の属性は既知の組み込み属性だけを
/// 許し、外側のモジュールと inherent impl のヘッダも含める。const fn にできない宣言
/// (本体の無い宣言、trait・trait の impl・extern ブロックの中) と、`async`・`default` や
/// rustc が受け付けない順序・重複の修飾子を持つ関数は `None` (証明しない)。
pub(crate) fn rust_const_qualifier(
    function: Node<'_>,
    source: &[u8],
) -> Option<RustConstQualifier> {
    if function.kind() != "function_item" {
        return None;
    }
    let own_name = function.child_by_field_name("name")?;
    let body = function.child_by_field_name("body")?;
    let mut const_keyword = None;
    let mut cursor = function.walk();
    if let Some(modifiers) = function
        .children(&mut cursor)
        .find(|child| child.kind() == "function_modifiers")
    {
        // 文法は修飾子を任意の順で受理するが、rustc は `const unsafe extern` の順に 1 つずつ
        // しか書かせない (`unsafe const fn` は構文エラー)。
        let mut previous = None;
        let mut cursor = modifiers.walk();
        for modifier in modifiers.children(&mut cursor) {
            if modifier.is_extra() {
                continue;
            }
            let order = match modifier.kind() {
                "const" => 0,
                "unsafe" => 1,
                "extern_modifier" => 2,
                // `async` は `const` と併用できず、`default` は specialization の構文。
                _ => return None,
            };
            if previous.is_some_and(|previous| previous >= order) {
                return None;
            }
            previous = Some(order);
            if order == 0 {
                const_keyword = Some(modifier.id());
            }
        }
    }
    let mut tokens = Vec::new();
    append_header(
        function,
        source,
        &HashSet::new(),
        const_keyword,
        &mut tokens,
    )?;
    // 戻り値の `impl Trait` の隠れた型 (外に漏れる auto trait を含む) は本体で決まるので、
    // そのときだけ本体も比べる。
    let opaque_return = function
        .child_by_field_name("return_type")
        .is_some_and(|ty| any_node(ty, Children::Named, |node| node.kind() == "abstract_type"));
    if opaque_return {
        tokens.push("<body>".to_string());
        append_item_tokens(body, source, &mut tokens)?;
    }
    let mut names = BTreeSet::new();
    let mut skipped = vec![own_name.id()];
    if !opaque_return {
        skipped.push(body.id());
    }
    collect_names(function, source, &skipped, &mut names)?;
    let mut current = function;
    // 外側の宣言をたどる深さを制限し、深い未対応の配置では証明を保留する。
    for _ in 0..64 {
        append_outer_attributes(current, source, &mut tokens)?;
        if let Some(body) = current.child_by_field_name("body") {
            append_inner_attributes(body, source, &mut tokens)?;
        }
        let parent = current.parent()?;
        match parent.kind() {
            "source_file" => {
                if parent.has_error() {
                    return None;
                }
                append_inner_attributes(parent, source, &mut tokens)?;
                // NFC で同じになる別の綴りの宣言を引き当てられないので証明しない。
                if names.iter().any(|name: &String| !name.is_ascii()) {
                    return None;
                }
                return Some(RustConstQualifier {
                    is_const: const_keyword.is_some(),
                    tokens,
                    names,
                    start_byte: function.start_byte(),
                });
            }
            "declaration_list" => current = parent,
            // trait のメソッドと trait の impl は const にできず、外側の generics と境界は
            // 呼び出し契約の一部なのでヘッダごと比べる。
            "mod_item" | "impl_item" if parent.child_by_field_name("trait").is_none() => {
                tokens.push("<enclosing-declaration>".to_string());
                append_header(parent, source, &HashSet::new(), None, &mut tokens)?;
                if let Some(body) = parent.child_by_field_name("body") {
                    collect_names(parent, source, &[body.id()], &mut names)?;
                }
                current = parent;
            }
            _ => return None,
        }
    }
    None
}

/// `node` のうち `skipped` の部分木を除いた範囲に現れる識別子を `names` に足す。
fn collect_names(
    node: Node<'_>,
    source: &[u8],
    skipped: &[usize],
    names: &mut BTreeSet<String>,
) -> Option<()> {
    let flow = walk_preorder(node, Children::All, |node, _| {
        if skipped.contains(&node.id()) {
            return ControlFlow::Continue(Walk::Skip);
        }
        if node.child_count() == 0
            && matches!(
                node.kind(),
                "identifier" | "type_identifier" | "primitive_type" | "field_identifier"
            )
        {
            let Ok(text) = node.utf8_text(source) else {
                return ControlFlow::Break(());
            };
            names.insert(name_key(text).to_string());
        }
        ControlFlow::Continue(Walk::Descend)
    });
    flow.is_continue().then_some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::parser;
    use crate::engine::tree_walk::{Children, Walk, walk_preorder};
    use crate::language::LangId;
    use std::ops::ControlFlow;

    /// `source` の最初の関数 `f` の宣言と、そのファイルの宣言。
    fn parse(source: &str) -> (Option<RustConstQualifier>, Option<RustFileItems>) {
        let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
        let function = match walk_preorder(tree.root_node(), Children::Named, |node, _| {
            if matches!(node.kind(), "function_item" | "function_signature_item")
                && node
                    .child_by_field_name("name")
                    .and_then(|name| name.utf8_text(source.as_bytes()).ok())
                    == Some("f")
            {
                ControlFlow::Break(node)
            } else {
                ControlFlow::Continue(Walk::Descend)
            }
        }) {
            ControlFlow::Break(node) => node,
            ControlFlow::Continue(()) => panic!("missing function f: {source}"),
        };
        (
            rust_const_qualifier(function, source.as_bytes()),
            RustFileItems::new(tree.root_node(), source.as_bytes()),
        )
    }

    fn qualifier(source: &str) -> Option<RustConstQualifier> {
        parse(source).0
    }

    /// `old` から `new` への変更を、`const` を足しただけと証明できるか。
    fn adds_const(old: &str, new: &str) -> bool {
        let (Some(old_qualifier), Some(old_items)) = parse(old) else {
            return false;
        };
        let (Some(new_qualifier), Some(new_items)) = parse(new) else {
            return false;
        };
        new_qualifier.only_adds_const_to(&new_items, &old_qualifier, &old_items)
    }

    #[test]
    fn adding_const_alone_is_proven() {
        for (old, new) in [
            (
                "pub fn f(x: u8) -> u8 { x }",
                "pub const fn f(x: u8) -> u8 { x }",
            ),
            ("fn f() {}", "const fn f() { let _ = 1; }"),
            (
                "pub unsafe fn f(p: *const u8) {}",
                "pub const unsafe fn f(p: *const u8) {}",
            ),
            (
                "pub extern \"C\" fn f() {}",
                "pub const extern \"C\" fn f() {}",
            ),
            (
                "pub fn f(\n    x: u8,\n) {}",
                "pub const fn f(x: u8 /* x */) {}",
            ),
            ("pub fn f(mut x: u8) {}", "pub const fn f(x: u8) {}"),
            (
                "/// doc\n#[inline]\n#[must_use]\npub fn f() -> u8 { 0 }",
                "/// doc changed\n#[inline]\n#[must_use]\npub const fn f() -> u8 { 1 }",
            ),
            (
                "pub struct S;\nimpl S {\n    pub fn f(&self) -> u8 { 0 }\n}",
                "pub struct S;\nimpl S {\n    pub const fn f(&self) -> u8 { 0 }\n}",
            ),
            (
                "pub mod m {\n    pub fn f<T: Copy>(x: T) -> T { x }\n}",
                "pub mod m {\n    pub const fn f<T: Copy>(x: T) -> T { x }\n}",
            ),
            ("pub fn f() {}", "pub const\nfn f() {}"),
            (
                "pub /* a */ unsafe fn f() {}",
                "pub /* a */ const /* b */ unsafe fn f() {}",
            ),
            // 宣言に現れない名前の宣言は、同時に変わっても関係しない。
            (
                "type Other = u8;\npub fn f(x: u8) {}",
                "type Other = u16;\npub const fn f(x: u8) {}",
            ),
            (
                "pub struct A;\npub struct B;\nimpl A {\n    pub fn f(&self) {}\n}\nimpl B {\n    pub fn f(&self) {}\n}",
                "pub struct A;\npub struct B;\nimpl A {\n    pub const fn f(&self) {}\n}\nimpl B {\n    pub const fn f(&self) {}\n}",
            ),
        ] {
            assert!(adds_const(old, new), "{old} -> {new}");
        }
    }

    /// ヘッダの字面が同じでも、そこに現れる名前の同じファイルの宣言が変われば型や値が変わる。
    #[test]
    fn changed_declarations_of_names_in_the_header_are_not_proven() {
        for (old, new) in [
            (
                "type Arg = u8;\npub fn f(_: Arg) {}",
                "type Arg = u16;\npub const fn f(_: Arg) {}",
            ),
            (
                "const N: usize = 4;\npub fn f(x: [u8; N]) {}",
                "const N: usize = 8;\npub const fn f(x: [u8; N]) {}",
            ),
            (
                "const fn size() -> usize { 4 }\npub fn f(x: [u8; size()]) {}",
                "const fn size() -> usize { 8 }\npub const fn f(x: [u8; size()]) {}",
            ),
            (
                "pub struct S;\nimpl S {\n    const N: usize = 4;\n}\npub fn f(x: [u8; S::N]) {}",
                "pub struct S;\nimpl S {\n    const N: usize = 8;\n}\npub const fn f(x: [u8; S::N]) {}",
            ),
            (
                "mod m {\n    pub type Arg = u8;\n}\npub fn f(_: m::Arg) {}",
                "mod m {\n    pub type Arg = u16;\n}\npub const fn f(_: m::Arg) {}",
            ),
            (
                "#[cfg(unix)]\ntype Arg = u8;\npub fn f(_: Arg) {}",
                "#[cfg(windows)]\ntype Arg = u8;\npub const fn f(_: Arg) {}",
            ),
            (
                "pub struct S {\n    a: u8,\n}\nimpl S {\n    pub fn f(&self) {}\n}",
                "pub struct S {\n    a: u16,\n}\nimpl S {\n    pub const fn f(&self) {}\n}",
            ),
            (
                "make_type!(Arg, u8);\npub fn f(_: Arg) {}",
                "make_type!(Arg, u16);\npub const fn f(_: Arg) {}",
            ),
            (
                "make_type! { Arg u8 }\npub fn f(_: Arg) {}",
                "make_type! { Arg u16 }\npub const fn f(_: Arg) {}",
            ),
            (
                "pub struct S;\nimpl S {\n    make_const!(N, 4);\n    pub fn f(&self) {}\n}",
                "pub struct S;\nimpl S {\n    make_const!(N, 8);\n    pub const fn f(&self) {}\n}",
            ),
            (
                "macro_rules! make {\n    ($n:ident) => { type $n = u8; };\n}\nmake!(Arg);\npub fn f(_: Arg) {}",
                "macro_rules! make {\n    ($n:ident) => { type $n = u16; };\n}\nmake!(Arg);\npub const fn f(_: Arg) {}",
            ),
            // 名前の宣言が使う名前の宣言 (型別名の連鎖・定数の式) もたどる。
            (
                "type Arg = Inner;\ntype Inner = u8;\npub fn f(_: Arg) {}",
                "type Arg = Inner;\ntype Inner = u16;\npub const fn f(_: Arg) {}",
            ),
            (
                "const BASE: usize = 2;\nconst N: usize = BASE * 2;\npub fn f(x: [u8; N]) {}",
                "const BASE: usize = 3;\nconst N: usize = BASE * 2;\npub const fn f(x: [u8; N]) {}",
            ),
            (
                "macro_rules! size { ($n:expr) => { $n }; }\nconst BASE: usize = 2;\nconst N: usize = size!(BASE);\npub fn f(_: [u8; N]) {}",
                "macro_rules! size { ($n:expr) => { $n }; }\nconst BASE: usize = 3;\nconst N: usize = size!(BASE);\npub const fn f(_: [u8; N]) {}",
            ),
            // 別のモジュールの同名の宣言との入れ替えも違いになる。
            (
                "mod a {\n    pub type Arg = u8;\n}\nmod b {\n    pub type Arg = u16;\n}\npub fn f(_: a::Arg) {}",
                "mod a {\n    pub type Arg = u16;\n}\nmod b {\n    pub type Arg = u8;\n}\npub const fn f(_: a::Arg) {}",
            ),
            // raw identifier は同じ名前として引く。
            (
                "type Arg = u8;\npub fn f(_: r#Arg) {}",
                "type Arg = u16;\npub const fn f(_: r#Arg) {}",
            ),
            (
                "type r#Arg = u8;\npub fn f(_: Arg) {}",
                "type r#Arg = u16;\npub const fn f(_: Arg) {}",
            ),
            // `use` の別名の束縛先もたどる。
            (
                "type Original = u8;\nuse self::Original as Arg;\npub fn f(_: Arg) {}",
                "type Original = u16;\nuse self::Original as Arg;\npub const fn f(_: Arg) {}",
            ),
            (
                "enum E {\n    N = 4,\n}\nuse E::N;\npub fn f(x: [u8; N as usize]) {}",
                "enum E {\n    N = 8,\n}\nuse E::N;\npub const fn f(x: [u8; N as usize]) {}",
            ),
            // 囲む impl の属性と、関数の本体の中の impl も外の型に効く。
            (
                "pub struct S;\npub trait T {\n    type A;\n}\n#[cfg(unix)]\nimpl T for S {\n    type A = u8;\n}\n#[cfg(not(unix))]\nimpl T for S {\n    type A = u16;\n}\npub fn f(_: <S as T>::A) {}",
                "pub struct S;\npub trait T {\n    type A;\n}\n#[cfg(not(unix))]\nimpl T for S {\n    type A = u8;\n}\n#[cfg(unix)]\nimpl T for S {\n    type A = u16;\n}\npub const fn f(_: <S as T>::A) {}",
            ),
            (
                "pub struct S;\npub trait T {\n    type A;\n}\nfn install() {\n    impl T for S {\n        type A = u8;\n    }\n}\npub fn f(_: <S as T>::A) {}",
                "pub struct S;\npub trait T {\n    type A;\n}\nfn install() {\n    impl T for S {\n        type A = u16;\n    }\n}\npub const fn f(_: <S as T>::A) {}",
            ),
            (
                "pub struct S;\npub trait T {\n    type A;\n}\nconst _: () = {\n    impl T for S {\n        type A = u8;\n    }\n};\npub fn f(_: <S as T>::A) {}",
                "pub struct S;\npub trait T {\n    type A;\n}\nconst _: () = {\n    impl T for S {\n        type A = u16;\n    }\n};\npub const fn f(_: <S as T>::A) {}",
            ),
            (
                "mod m {\n    #![cfg(unix)]\n    pub type Arg = u8;\n}\npub fn f(_: m::Arg) {}",
                "mod m {\n    #![cfg(windows)]\n    pub type Arg = u8;\n}\npub const fn f(_: m::Arg) {}",
            ),
            // 戻り値の `impl Trait` の隠れた型は本体で決まる。
            (
                "pub fn f() -> impl Copy { 0u8 }",
                "pub const fn f() -> impl Copy { core::marker::PhantomData::<*const ()> }",
            ),
            // NFC で同じになる綴り (結合文字・ケルビン記号) は引き当てられないので証明しない。
            (
                "type Caf\u{e9} = u8;\npub fn f(_: Cafe\u{301}) {}",
                "type Caf\u{e9} = u16;\npub const fn f(_: Cafe\u{301}) {}",
            ),
            (
                "type \u{212a}ind = u8;\npub fn f(_: Kind) {}",
                "type \u{212a}ind = u16;\npub const fn f(_: Kind) {}",
            ),
        ] {
            assert!(!adds_const(old, new), "{old} -> {new}");
        }
    }

    /// 戻り値が `impl Trait` でも、本体が同じなら `const` を足しただけと証明する。
    #[test]
    fn opaque_return_type_with_unchanged_body_is_proven() {
        assert!(adds_const(
            "pub fn f() -> impl Copy { 0u8 }",
            "pub const fn f() -> impl Copy { 0u8 }"
        ));
    }

    #[test]
    fn opaque_return_type_requires_unchanged_body_dependencies() {
        let old = "const VALUE: u8 = 0;\npub fn f() -> impl Copy { VALUE }";
        assert!(adds_const(old, &old.replace("pub fn", "pub const fn")));
        assert!(!adds_const(
            old,
            "const VALUE: *const () = core::ptr::null();\npub const fn f() -> impl Copy { VALUE }",
        ));
    }

    #[test]
    fn enclosing_attributes_of_nested_impls_are_part_of_the_contract() {
        let source = |small: &str, large: &str, qualifier: &str| {
            format!(
                "pub struct S;\npub trait T {{ type A; }}\n#[cfg({small})]\nfn small() {{ impl T for S {{ type A = u8; }} }}\n#[cfg({large})]\nfn large() {{ impl T for S {{ type A = u16; }} }}\npub {qualifier}fn f(_: <S as T>::A) {{}}"
            )
        };
        let old = source("all()", "any()", "");
        assert!(adds_const(&old, &source("all()", "any()", "const ")));
        assert!(!adds_const(&old, &source("any()", "all()", "const ")));
    }

    #[test]
    fn macro_token_tree_and_literal_contents_are_preserved() {
        for (old_value, new_value) in [
            ("stringify!(a + b).len()", "stringify!(a+b).len()"),
            ("\"a  b\".len()", "\"a b\".len()"),
            ("r#\"a  b\"#.len()", "r#\"a b\"#.len()"),
        ] {
            assert!(!adds_const(
                &format!("const N: usize = {old_value};\npub fn f(_: [u8; N]) {{}}"),
                &format!("const N: usize = {new_value};\npub const fn f(_: [u8; N]) {{}}"),
            ));
        }
    }

    #[test]
    fn review_regression_empty_impl_removal_is_not_compatible() {
        let old = "#[derive(Clone, Copy)]\npub struct Token(*const ());\nunsafe impl Send for Token {}\npub fn f() -> impl Copy { Token(core::ptr::null()) }";
        let new = old
            .replace("unsafe impl Send for Token {}\n", "")
            .replace("pub fn f", "pub const fn f");
        assert!(adds_const(old, &old.replace("pub fn f", "pub const fn f")));
        assert!(!adds_const(old, &new));
    }

    #[test]
    fn review_regression_nested_macro_invocation_is_not_compatible() {
        let old = "pub struct S;\npub trait T { type A; }\nmacro_rules! install { ($ty:ty) => { impl T for S { type A = $ty; } }; }\nfn configure() { install!(u8); }\npub fn f(_: <S as T>::A) {}";
        let new = old
            .replace("install!(u8)", "install!(u16)")
            .replace("pub fn f", "pub const fn f");
        assert!(adds_const(old, &old.replace("pub fn f", "pub const fn f")));
        assert!(!adds_const(old, &new));
    }

    #[test]
    fn review_regression_opaque_macro_dependencies_are_not_compatible() {
        let old = "const N: usize = 4;\nmacro_rules! define_arg { () => { type Arg = [u8; N]; }; }\ndefine_arg!();\npub fn f(_: Arg) {}";
        let new = old
            .replace("N: usize = 4", "N: usize = 8")
            .replace("pub fn f", "pub const fn f");
        assert!(adds_const(old, &old.replace("pub fn f", "pub const fn f")));
        assert!(!adds_const(old, &new));
    }

    #[test]
    fn review_regression_own_macro_keeps_const_addition_compatible() {
        for old in [
            "pub fn f(value: u8) -> u8 { assert!(value > 0); value }",
            "#[inline]\npub fn f(value: u8) -> u8 { assert!(value > 0); value }",
            "pub struct S;\nimpl S { pub fn f(&self, value: u8) -> u8 { assert!(value > 0); value } }",
        ] {
            assert!(adds_const(old, &old.replace("pub fn f", "pub const fn f")));
        }
    }

    #[test]
    fn review_regression_batched_const_additions_skip_only_declaration_names() {
        let old = "pub fn f() { assert!(true); }\npub fn g() { assert!(true); }";
        assert!(adds_const(old, &old.replace("pub fn", "pub const fn")));

        let old = "pub fn f() { assert!(true); let _ = g(); }\nconst fn g() -> u8 { 0 }";
        let new = old
            .replace("pub fn f", "pub const fn f")
            .replace("fn g() -> u8 { 0 }", "fn g() -> u16 { 0 }");
        assert!(!adds_const(old, &new));
    }

    #[test]
    fn review_regression_const_dependencies_allow_only_added_qualifier() {
        let old = "pub fn f() { assert!(true); g(); }\npub fn g() { assert!(true); }";
        assert!(adds_const(old, &old.replace("pub fn", "pub const fn")));

        let old = "pub fn f() { assert!(true); let _ = g(); }\npub fn g() -> u8 { 0 }";
        for dependency in [
            "pub const fn g() -> u8 { 1 }",
            "pub const fn g() -> u16 { 0 }",
            "pub const fn g(value: u8) -> u8 { 0 }",
            "#[must_use]\npub const fn g() -> u8 { 0 }",
        ] {
            let new = format!("pub const fn f() {{ assert!(true); let _ = g(); }}\n{dependency}");
            assert!(!adds_const(old, &new), "{new}");
        }
        let old = old.replace("pub fn g", "pub const fn g");
        let new = old
            .replace("pub fn f", "pub const fn f")
            .replace("pub const fn g", "pub fn g");
        assert!(!adds_const(&old, &new));
    }

    #[test]
    fn review_regression_const_macro_headers_keep_normalization() {
        for (old, new) in [
            (
                "pub fn f(mut value: u8) -> u8 { assert!(value > 0); value }",
                "pub const fn f(value: u8) -> u8 { assert!(value > 0); value }",
            ),
            (
                "pub fn f(value: u8,) -> u8 { assert!(value > 0); value }",
                "pub const fn f(value: u8) -> u8 { assert!(value > 0); value }",
            ),
            (
                "pub fn f(value: u8) -> u8 { assert!(value > 0); value }",
                "pub const fn f(value: u8,) -> u8 { assert!(value > 0); value }",
            ),
        ] {
            assert!(adds_const(old, new), "{old} -> {new}");
        }
        assert!(!adds_const(
            "pub fn f(mut value: u8) -> u8 { assert!(value > 0); value }",
            "pub const fn f(value: u8) -> u8 { assert!(value >= 0); value }",
        ));
    }

    #[test]
    fn review_regression_location_macros_require_unchanged_positions() {
        for (prefix, invocation) in [
            ("", "line!()"),
            ("", "column!()"),
            ("use core::line as here;\n", "here!()"),
            ("use core::line as assert;\n", "assert!()"),
            ("macro_rules! here { () => { line!() }; }\n", "here!()"),
            ("macro_rules! assert { () => { line!() }; }\n", "assert!()"),
            (
                "macro_rules! here { ($value:expr) => { $value }; }\n",
                "here!(line!())",
            ),
        ] {
            let old = format!(
                "{prefix}pub struct S;\npub fn f(_: [u8; S::N]) {{ impl S {{ const N: usize = {invocation} as usize; }} }}"
            );
            for header in ["pub const fn f", "pub const\nfn f"] {
                assert!(
                    !adds_const(&old, &old.replace("pub fn f", header)),
                    "{old} -> {header}"
                );
            }
        }
        for invocation in ["line!()", "column!()"] {
            let old = format!(
                "pub struct S;\npub fn f(_: [u8; S::N]) {{\n    impl S {{ const N: usize = {invocation} as usize; }}\n}}"
            );
            assert!(adds_const(&old, &old.replace("pub fn f", "pub const fn f")));
        }
        let old = "pub struct S;\npub fn f(_: [u8; S::N]) { /*        */ impl S { const N: usize = column!() as usize; } }";
        let new = old
            .replace("pub fn f", "pub const fn f")
            .replace("/*        */", "/*é*/");
        assert!(!adds_const(old, &new));
    }

    #[test]
    fn review_regression_location_macros_in_headers_stay_unproven() {
        for invocation in ["line!()", "column!()"] {
            let old = format!("pub fn f(_: [u8; {invocation} as usize]) {{}}");
            for header in ["pub const fn f", "pub const\nfn f"] {
                assert!(!adds_const(&old, &old.replace("pub fn f", header)));
            }
        }
    }

    #[test]
    fn other_differences_and_removal_are_not_proven() {
        for (old, new) in [
            ("pub const fn f() {}", "pub fn f() {}"),
            ("pub const fn f() {}", "pub const fn f() {}"),
            ("pub fn f() {}", "pub fn f() {}"),
            ("pub fn f(x: u8) {}", "pub const fn f(x: u8, y: u8) {}"),
            ("pub fn f(x: u8) {}", "pub const fn f(_x: u8) {}"),
            ("pub fn f(x: u8) {}", "pub const fn f(x: u16) {}"),
            ("pub fn f() -> u8 { 0 }", "pub const fn f() -> u16 { 0 }"),
            ("pub fn f() {}", "pub const fn f<T>() {}"),
            ("pub fn f<T: Copy>() {}", "pub const fn f<T: Clone>() {}"),
            (
                "pub fn f<T>() where T: Copy {}",
                "pub const fn f<T>() where T: Clone {}",
            ),
            ("pub fn f() {}", "pub const unsafe fn f() {}"),
            ("pub fn f() {}", "pub const extern \"C\" fn f() {}"),
            (
                "pub extern \"C\" fn f() {}",
                "pub const extern \"system\" fn f() {}",
            ),
            ("pub fn f() {}", "pub(crate) const fn f() {}"),
            ("fn f() {}", "pub const fn f() {}"),
            ("#[inline]\npub fn f() {}", "#[cold]\npub const fn f() {}"),
            ("pub fn f() {}", "#[must_use]\npub const fn f() {}"),
            (
                "pub struct S<T>(T);\nimpl<T: Copy> S<T> {\n    pub fn f(&self) {}\n}",
                "pub struct S<T>(T);\nimpl<T: Clone> S<T> {\n    pub const fn f(&self) {}\n}",
            ),
            (
                "pub struct S;\nimpl S {\n    pub fn f(&self) {}\n}",
                "pub struct S;\nimpl S {\n    pub const fn f(&mut self) {}\n}",
            ),
            (
                "mod m {\n    pub fn f() {}\n}",
                "pub mod m {\n    pub const fn f() {}\n}",
            ),
        ] {
            assert!(!adds_const(old, new), "{old} -> {new}");
        }
    }

    #[test]
    fn declarations_that_cannot_be_const_have_no_qualifier() {
        for source in [
            "pub async fn f() {}",
            "pub const async fn f() {}",
            "pub unsafe const fn f() {}",
            "pub extern \"C\" const fn f() {}",
            "pub extern \"C\" unsafe fn f() {}",
            "pub const const fn f() {}",
            "pub unsafe unsafe fn f() {}",
            "default fn f() {}",
            "pub trait T {\n    fn f(&self) {}\n}",
            "pub trait T {\n    fn f(&self);\n}",
            "pub struct S;\npub trait T {\n    fn f(&self);\n}\nimpl T for S {\n    fn f(&self) {}\n}",
            "extern \"C\" {\n    fn f();\n}",
            "fn outer() {\n    fn f() {}\n}",
            "#[instrument]\npub fn f() {}",
            "#[cfg_attr(test, inline)]\npub fn f() {}",
            "#[my_macro]\nimpl S {\n    pub fn f(&self) {}\n}",
            "pub fn f() { #![custom] }",
            "#![custom]\npub fn f() {}",
            "pub fn f(#[custom] x: u8) {}",
            "pub fn f() {}\nfn broken( {}",
        ] {
            assert!(qualifier(source).is_none(), "{source}");
        }
    }

    /// 関数自身の名前は、宣言の位置のほかにも現れるときだけ同じファイルの宣言を比べる。
    #[test]
    fn own_name_is_compared_only_when_used_elsewhere_in_the_declaration() {
        let names = |source: &str| qualifier(source).unwrap().names;
        assert!(names("pub fn f(x: u8) -> u8 { x }").contains("u8"));
        assert!(!names("pub struct S;\nimpl S {\n    pub fn f(&self) {}\n}").contains("f"));
        assert!(names("pub struct S;\nimpl S {\n    pub fn f(&self) {}\n}").contains("S"));
        assert!(names("pub fn f(x: [u8; f::N]) {}").contains("f"));
    }

    #[test]
    fn grammar_supports_the_nodes_used_by_the_proof() {
        let language = LangId::Rust.ts_language();
        tree_sitter::Query::new(
            &language,
            "(function_item (function_modifiers \"const\" \"async\" \"default\" \"unsafe\" (extern_modifier))) (function_item return_type: (abstract_type)) (impl_item trait: (_)) (impl_item body: (declaration_list)) (mod_item body: (declaration_list)) (trait_item body: (declaration_list)) (foreign_mod_item body: (declaration_list)) (use_declaration) (extern_crate_declaration) (inner_attribute_item) (attribute_item) (macro_definition name: (identifier) (macro_rule right: (token_tree))) (expression_statement (macro_invocation macro: (_))) (token_tree) (token_tree_pattern) (identifier) (type_identifier) (primitive_type) (field_identifier)",
        )
        .unwrap();
    }
}
