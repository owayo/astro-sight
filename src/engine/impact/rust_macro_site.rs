//! Rust の参照の名前を、参照元のクレートのそのモジュールで解決すると構文から言えるか (Issue #472)。
//!
//! 非公開の関数の変更で別のクレートの同名の参照を除くには、その参照が参照元のクレートの中で
//! コンパイルされ、そこで名前を解決することが前提になる。`macro_rules!` の本体の名前は展開した場所で
//! 解決し、マクロの引数と属性マクロを付けた項目は、別のクレートで展開されるコード (`#[macro_export]`
//! のマクロの本体・proc-macro の出力) へ運ばれ得るので、展開の規則を確かめたマクロの、確かめた位置
//! だけを除く。

use tree_sitter::Node;

/// 参照の位置の、名前を解決する場所の分類。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum MacroSite {
    /// マクロの外の普通の式で、囲む項目・文に組み込みでない属性 (属性マクロ・`cfg_attr`・import で
    /// 付け替えられる `#[test]` など) が無い。
    #[default]
    Plain,
    /// 裸の名前で呼んだ objc2 の `define_class!` の入力の、`impl` の中のメソッドの本体 (ほかのマクロの
    /// 呼び出しの中でも、属性の中でも、属性を付けた項目・文の中でもない)。objc2 0.6 は本体のブロックを、
    /// 呼び出したモジュールに生成するメソッドで評価するので、名前は参照元のモジュールで解決する。
    /// マクロが objc2 のものかは [`define_class_imported`]・[`rebinds_objc2_names`] と依存の確認で
    /// 別に確かめる。
    DefineClassBody,
    /// `macro_rules!` の右辺。名前は定義ファイルではなく展開先で解決し得る。
    RuleBody,
    /// 字句どおりに宣言または呼び出され、値の名前空間を使わないマクロ名。
    RuleMacroName,
    /// ほかのマクロの引数・属性マクロを付けた項目など。
    Unknown,
}

/// `identifier` (参照の識別子) の位置の分類。
pub(super) fn macro_site(identifier: Node<'_>, source: &[u8]) -> MacroSite {
    let mut site = MacroSite::Plain;
    let mut current = identifier;
    if identifier
        .parent()
        .is_some_and(|parent| matches!(parent.kind(), "token_tree" | "token_repetition"))
    {
        let invocation = match token_tree_site(identifier, source) {
            Ok(invocation) => invocation,
            Err(site) => return site,
        };
        site = MacroSite::DefineClassBody;
        current = invocation;
    }
    loop {
        if !preceding_attributes_safe(current, source) {
            return MacroSite::Unknown;
        }
        let Some(parent) = current.parent() else {
            return site;
        };
        match parent.kind() {
            "macro_definition"
            | "macro_invocation"
            | "token_tree"
            | "attribute_item"
            | "inner_attribute_item" => return MacroSite::Unknown,
            _ => {}
        }
        if !inner_attributes_safe(parent, source) {
            return MacroSite::Unknown;
        }
        current = parent;
    }
}

/// 組み込みの属性で、import で付け替えられないもの (`#[test]`・`#[derive(..)]` は同じ名前の属性マクロを
/// import すればそちらが動くので外す)。
fn builtin_attribute(attribute: Node<'_>, source: &[u8]) -> bool {
    if let Some(names) = crate::engine::impact::rust_std_origin::safe_attribute(attribute, source) {
        return names.is_empty();
    }
    let Some(attr) = attribute.named_child(0) else {
        return false;
    };
    if attr.has_error()
        || !attr
            .named_child(0)
            .is_some_and(|name| name.utf8_text(source) == Ok("cfg_attr"))
    {
        return false;
    }
    let Some(mut tokens) = attr
        .child_by_field_name("arguments")
        .and_then(|tree| delimited(tree, "("))
    else {
        return false;
    };
    tokens.retain(|token| !comment(*token));
    let mut arguments = tokens.split(|token| token.kind() == ",");
    // any() は常に偽。その他の条件では、適用され得る全属性を確かめる。
    if matches!(arguments.next(), Some([any, empty]) if any.utf8_text(source) == Ok("any")
        && delimited(*empty, "(").is_some_and(|inner| inner.iter().all(|n| comment(*n))))
    {
        return true;
    }
    arguments.all(|argument| {
        let name = match argument {
            [] => return true,
            [name] => name,
            [name, args]
                if args.kind() == "token_tree"
                    && args.child(0).is_some_and(|open| open.kind() == "(") =>
            {
                name
            }
            _ => return false,
        };
        name.utf8_text(source)
            .is_ok_and(super::rust_std_origin::inert_attribute_name)
    })
}

/// `node` の直前に並ぶ外側の属性 (`#[..]`) が、どれも組み込みの安全な属性か。
fn preceding_attributes_safe(node: Node<'_>, source: &[u8]) -> bool {
    let mut previous = node.prev_named_sibling();
    while let Some(sibling) = previous {
        match sibling.kind() {
            "line_comment" | "block_comment" => {}
            "attribute_item" => {
                if !builtin_attribute(sibling, source) {
                    return false;
                }
            }
            _ => return true,
        }
        previous = sibling.prev_named_sibling();
    }
    true
}

/// `scope` (ファイル・ブロック・`mod`・`impl` の本体) の内側の属性 (`#![..]`) が、どれも組み込みの
/// 安全な属性か。
fn inner_attributes_safe(scope: Node<'_>, source: &[u8]) -> bool {
    let mut cursor = scope.walk();
    scope
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "inner_attribute_item")
        .all(|attribute| builtin_attribute(attribute, source))
}

/// 未知の属性が書き換え得る範囲。前順で範囲全体を飛ばすので、範囲は整列済みで重ならない。
#[derive(Default)]
pub(super) struct OpaqueAttributeScopes {
    spans: Vec<std::ops::Range<usize>>,
    token_calls: Vec<usize>,
}

impl OpaqueAttributeScopes {
    pub(super) fn build(mut root: Node<'_>, source: &[u8]) -> Self {
        use crate::engine::tree_walk::{Children, Walk, walk_preorder};
        use std::ops::ControlFlow;

        while let Some(parent) = root.parent() {
            root = parent;
        }
        let mut scopes = Self::default();
        if root.has_error() {
            scopes.spans.push(root.byte_range());
            scopes.collect_token_calls(root, source);
        } else if memchr::memchr(b'#', source).is_some()
            || memchr::memmem::find(source, b"macro_rules").is_some()
        {
            let mut pending = Vec::new();
            let _: ControlFlow<()> = walk_preorder(root, Children::Named, |node, depth| {
                pending.resize(depth + 1, false);
                let action = match node.kind() {
                    "attribute_item" => {
                        // macro_export は公開先を変えるが、規則本体のドット呼び出しは書き換えない。
                        let exports_rule = node
                            .named_child(0)
                            .and_then(|attr| attr.named_child(0))
                            .is_some_and(|name| name.utf8_text(source) == Ok("macro_export"));
                        pending[depth] |= !exports_rule && !builtin_attribute(node, source);
                        scopes.spans.push(node.byte_range());
                        Walk::Skip
                    }
                    "inner_attribute_item" | "macro_definition" => {
                        if std::mem::take(&mut pending[depth]) && node.kind() == "macro_definition"
                        {
                            scopes.collect_token_calls(node, source);
                        }
                        scopes.spans.push(node.byte_range());
                        Walk::Skip
                    }
                    "line_comment" | "block_comment" => Walk::Skip,
                    _ if std::mem::take(&mut pending[depth])
                        || !inner_attributes_safe(node, source) =>
                    {
                        scopes.spans.push(node.byte_range());
                        scopes.collect_token_calls(node, source);
                        Walk::Skip
                    }
                    _ => Walk::Descend,
                };
                ControlFlow::Continue(action)
            });
        }
        scopes.token_calls.sort_unstable();
        scopes
    }

    pub(super) fn contains(&self, node: Node<'_>) -> bool {
        let start = node.start_byte();
        let after = self.spans.partition_point(|span| span.end <= start);
        self.spans
            .get(after)
            .is_some_and(|span| span.contains(&start))
    }

    pub(super) fn rewrites_token_call(&self, node: Node<'_>) -> bool {
        self.token_calls.binary_search(&node.start_byte()).is_ok()
    }

    /// 属性が受け取る項目の中では、別のマクロの引数も属性の入力になる。
    /// 各 token tree の直下を一度だけ読み、`.name(...)`・`.name::<...>` の名前を記録する。
    fn collect_token_calls(&mut self, root: Node<'_>, source: &[u8]) {
        use crate::engine::tree_walk::{Children, Walk, walk_preorder};
        use std::ops::ControlFlow;

        if memchr::memchr(b'!', &source[root.byte_range()]).is_none() {
            return;
        }
        let _: ControlFlow<()> = walk_preorder(root, Children::Named, |node, _| {
            if matches!(node.kind(), "attribute_item" | "inner_attribute_item") {
                return ControlFlow::Continue(Walk::Skip);
            }
            if node.kind() == "token_tree" {
                let mut dot = false;
                let mut member = None;
                let mut cursor = node.walk();
                let mut tokens = node
                    .children(&mut cursor)
                    .filter(|n| !comment(*n))
                    .peekable();
                while let Some(token) = tokens.next() {
                    if let Some(offset) = member.take()
                        && (token.kind() == "::"
                            && tokens.peek().is_some_and(|next| next.kind() == "<")
                            || token.kind() == "token_tree"
                                && token.child(0).is_some_and(|open| open.kind() == "("))
                    {
                        self.token_calls.push(offset);
                    }
                    if dot && token.kind() == "identifier" {
                        member = Some(token.start_byte());
                    }
                    dot = token.kind() == ".";
                }
            }
            ControlFlow::Continue(Walk::Descend)
        });
    }
}

/// token tree の列を 1 度たどり、マクロの右辺か、呼び出した場所で解決する既知の入力かを区別する。
fn token_tree_site<'tree>(
    identifier: Node<'tree>,
    source: &[u8],
) -> Result<Node<'tree>, MacroSite> {
    let mut chain = Vec::new();
    let mut current = identifier;
    let invocation = loop {
        let parent = current.parent().ok_or(MacroSite::Unknown)?;
        match parent.kind() {
            "token_tree" | "token_repetition" => chain.push(parent),
            "macro_invocation" => break parent,
            "macro_rule" if parent.child_by_field_name("right") == Some(current) => {
                return Err(rule_name_site(identifier, source, &chain));
            }
            _ => return Err(MacroSite::Unknown),
        }
        current = parent;
    };
    define_class_body(identifier, source, invocation, &chain).ok_or(MacroSite::Unknown)?;
    Ok(invocation)
}

/// ファイル直下にあるただ一つの非公開マクロが、同じファイルの無条件の関数を呼ぶ位置。
/// 外から使えるマクロや別の展開先を作り得る構文、展開先での名前の再束縛があれば証明しない。
pub(super) fn closed_local_rule_call(root: Node<'_>, source: &[u8]) -> Option<usize> {
    if root.has_error() {
        return None;
    }
    let mut definition = None;
    let mut functions = Vec::new();
    let mut cursor = root.walk();
    for item in root.named_children(&mut cursor) {
        match item.kind() {
            "function_item" => functions.push(item),
            "macro_definition" if definition.is_none() => definition = Some(item),
            "line_comment" | "block_comment" => {}
            _ => return None,
        }
    }
    let definition = definition?;
    let macro_name = definition
        .child_by_field_name("name")?
        .utf8_text(source)
        .ok()?;
    let mut rules = definition
        .named_children(&mut cursor)
        .filter(|item| item.kind() == "macro_rule");
    let rule = rules.next()?;
    let pattern = rule.child_by_field_name("left")?;
    if rules.next().is_some() || pattern.child_count() != 2 || pattern.child(0)?.kind() != "(" {
        return None;
    }
    let body = delimited(rule.child_by_field_name("right")?, "{")?;
    let [callee, arguments] = body.as_slice() else {
        return None;
    };
    let name = callee.utf8_text(source).ok()?.trim_start_matches("r#");
    if callee.kind() != "identifier"
        || !delimited(*arguments, "(")?
            .iter()
            .all(|node| node.kind() == "," || node.kind().ends_with("_literal"))
    {
        return None;
    }
    let mut declarations = 0;
    for function in functions {
        let declared = function.child_by_field_name("name")?;
        declarations +=
            usize::from(declared.utf8_text(source).ok()?.trim_start_matches("r#") == name);
        let open = crate::engine::tree_walk::any_node(
            function,
            crate::engine::tree_walk::Children::Named,
            |node| match node.kind() {
                "use_declaration"
                | "extern_crate_declaration"
                | "mod_item"
                | "attribute_item"
                | "inner_attribute_item"
                | "macro_definition" => true,
                "macro_invocation" => !node.child_by_field_name("macro").is_some_and(|callee| {
                    callee.kind() == "identifier"
                        && callee.utf8_text(source).ok() == Some(macro_name)
                }),
                kind => {
                    kind.ends_with("identifier")
                        && node != declared
                        && node
                            .utf8_text(source)
                            .is_ok_and(|word| word.trim_start_matches("r#") == name)
                }
            },
        );
        if open {
            return None;
        }
    }
    (declarations == 1).then_some(callee.start_byte())
}

/// メンバー名・フィールドのラベルと `$crate` の固定パスは展開先の値の名前に解決しない。
/// 別のマクロの入力にあるトークンは、受け手が修飾子や `.`・`:` を分離し得る。
fn rule_name_site(identifier: Node<'_>, source: &[u8], chain: &[Node<'_>]) -> MacroSite {
    let fixed = previous_token(identifier).is_some_and(|previous| previous.kind() == ".")
        || next_token(identifier).is_some_and(|next| next.kind() == ":")
        || definition_crate_path(identifier, source);
    if fixed
        && chain.split_last().is_some_and(|(_, nested)| {
            !nested.iter().any(|tree| {
                previous_token(*tree).is_some_and(|previous| {
                    matches!(
                        previous.kind(),
                        "!" | "#" | "metavariable" | "token_repetition"
                    )
                })
            })
        })
    {
        MacroSite::Unknown
    } else {
        MacroSite::RuleBody
    }
}

/// `$crate::module::name` の固定パスは、展開先の import を使わない。
fn definition_crate_path(mut name: Node<'_>, source: &[u8]) -> bool {
    while let Some(prefix) = previous_token(name)
        .filter(|separator| separator.kind() == "::")
        .and_then(previous_token)
    {
        if prefix.kind() != "identifier" {
            return prefix.kind() == "metavariable"
                && prefix.utf8_text(source).ok() == Some("$crate");
        }
        name = prefix;
    }
    false
}

/// 参照の識別子が、裸の名前で呼んだ `define_class!` の入力の `impl` のメソッドの本体にあれば、その
/// 呼び出し。`objc2::define_class!` は扱わない (先頭の `objc2` はモジュールの束縛・glob の import・
/// マクロが作る束縛で付け替えられ、import の経路のような曖昧さの検査が無い)。
fn define_class_body(
    identifier: Node<'_>,
    source: &[u8],
    invocation: Node<'_>,
    chain: &[Node<'_>],
) -> Option<()> {
    let name = invocation.child_by_field_name("macro")?;
    if name.kind() != "identifier" || name.utf8_text(source).ok()? != "define_class" {
        return None;
    }
    // ファイル直下の呼び出しだけ (ファイル直下の `use objc2::define_class` が届くのはそこだけで、
    // 中の `mod` や関数の中では別の束縛 (マクロが作る `macro_rules!` を含む) を指し得る)。
    // ファイル直下の `define_class!(..);` は式の文になる。
    let statement = invocation.parent()?;
    let top_level = match statement.kind() {
        "source_file" => true,
        "expression_statement" => statement
            .parent()
            .is_some_and(|file| file.kind() == "source_file"),
        _ => false,
    };
    if !top_level {
        return None;
    }
    // 参照の識別子がマクロの名前 (`translate!`・`translate /* .. */ !`) なら関数の参照でない。
    if next_token(identifier).is_some_and(|next| next.kind() == "!") {
        return None;
    }
    let [nested @ .., method_body, impl_body, _arguments] = chain else {
        return None;
    };
    if !braced(*impl_body) || !impl_header(*impl_body)? {
        return None;
    }
    if !braced(*method_body) || !method_header(*method_body, source)? {
        return None;
    }
    // 本体の中の、ほかのマクロの引数 (`name!(..)`・`name! /* .. */ (..)`) と属性 (`#[..]`) の token
    // tree は運ばれ得る。
    if nested.iter().any(|tree| {
        previous_token(*tree).is_some_and(|previous| matches!(previous.kind(), "!" | "#"))
    }) {
        return None;
    }
    // 本体と参照を囲む token tree の直下の属性 (`#[emit] fn inner() { .. }`・`#![..]`) は、参照を含む
    // 項目・文を属性マクロへ渡し得る。直下の `macro_rules!` は、参照をマクロの本体にする (展開した
    // 場所で名前を解決する。`#[macro_export]` を別のマクロから渡し得る)。
    if std::iter::once(method_body).chain(nested).any(|tree| {
        has_child(*tree, "#") || child_identifiers(*tree, source).any(|name| name == "macro_rules")
    }) {
        return None;
    }
    Some(())
}

/// token tree が `{ .. }` か。
fn braced(tree: Node<'_>) -> bool {
    tree.child(0).is_some_and(|open| open.kind() == "{")
}

/// `node` の直下に `kind` の子があるか。
fn has_child(node: Node<'_>, kind: &str) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|child| child.kind() == kind)
}

/// `node` の直下の識別子の綴り (`r#` を外す)。
fn child_identifiers<'a>(node: Node<'a>, source: &'a [u8]) -> impl Iterator<Item = &'a str> + 'a {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .filter(|child| child.kind() == "identifier")
        .filter_map(move |child| child.utf8_text(source).ok())
        .map(|name| name.trim_start_matches("r#"))
        .collect::<Vec<_>>()
        .into_iter()
}

fn comment(node: Node<'_>) -> bool {
    matches!(node.kind(), "line_comment" | "block_comment")
}

/// コメントを飛ばした次の兄弟。
fn next_token<'tree>(node: Node<'tree>) -> Option<Node<'tree>> {
    let mut next = node.next_sibling();
    while let Some(sibling) = next {
        if !comment(sibling) {
            return Some(sibling);
        }
        next = sibling.next_sibling();
    }
    None
}

/// コメントを飛ばした前の兄弟。
fn previous_token<'tree>(node: Node<'tree>) -> Option<Node<'tree>> {
    let mut previous = node.prev_sibling();
    while let Some(sibling) = previous {
        if !comment(sibling) {
            return Some(sibling);
        }
        previous = sibling.prev_sibling();
    }
    None
}

/// `{ .. }` の token tree の前の見出し (前の項目との区切り `;`・`{ .. }`・開き括弧の後から。コメントを
/// 除く)。`tree` 自体か区切りに見える `{ .. }` が `<`・`,`・`=`・`!` の後 (const 引数の `R<{1}, fn()>`・
/// 既定値・型の位置のマクロ `R<ty!{}, fn()>`・`-> ret! { .. }`) なら、見出しの途中なので `None`
/// (`-> !` の本体も `None` にする)。
fn header(tree: Node<'_>) -> Option<Vec<Node<'_>>> {
    if previous_token(tree).is_some_and(|before| matches!(before.kind(), "<" | "," | "=" | "!")) {
        return None;
    }
    let mut tokens = Vec::new();
    let mut previous = previous_token(tree);
    while let Some(node) = previous {
        match node.kind() {
            ";" | "{" | "(" | "[" => break,
            "token_tree" if braced(node) => {
                if previous_token(node)
                    .is_some_and(|before| matches!(before.kind(), "<" | "," | "=" | "!"))
                {
                    return None;
                }
                break;
            }
            _ => tokens.push(node),
        }
        previous = previous_token(node);
    }
    tokens.reverse();
    Some(tokens)
}

/// `impl` の本体の見出しが `impl` を含み、属性 (`#[..]`) と `!` を持たないか (objc2 は `impl` の属性を
/// 生成する `impl` に付ける。`impl ret! { .. } { .. }` の見出しの `!` は、本体に見える波括弧をマクロの
/// 引数にし得る)。見出しを決められなければ `None`。
fn impl_header(body: Node<'_>) -> Option<bool> {
    let header = header(body)?;
    Some(
        header.iter().any(|token| token.kind() == "impl")
            && !header.iter().any(|token| matches!(token.kind(), "#" | "!")),
    )
}

/// メソッドの本体の見出しが `fn` を含み、属性が objc2 のメソッドの印 (`#[unsafe(method(..))]`・
/// `#[unsafe(method_id(..))]`。`define_class!` が読んで消す) だけで、`!` (戻り値の型の位置のマクロ
/// `-> ret!(..)`・`-> !`) を持たないか。ほかの属性は生成するメソッドに付く。見出しを決められなければ
/// `None`。
fn method_header(body: Node<'_>, source: &[u8]) -> Option<bool> {
    let header = header(body)?;
    Some(
        header.iter().any(|token| token.kind() == "fn")
            && !header.iter().any(|token| token.kind() == "!")
            && header.iter().enumerate().all(|(index, token)| {
                token.kind() != "#"
                    || header
                        .get(index + 1)
                        .is_some_and(|attribute| objc2_method_marker(*attribute, source))
            }),
    )
}

/// `[unsafe(method(..))]`・`[unsafe(method_id(..))]` の token tree か (間にコメントがあれば偽)。
fn objc2_method_marker(attribute: Node<'_>, source: &[u8]) -> bool {
    let Some(outer) = delimited(attribute, "[") else {
        return false;
    };
    let [marker, arguments] = outer.as_slice() else {
        return false;
    };
    if marker.utf8_text(source).ok() != Some("unsafe") {
        return false;
    }
    let Some(inner) = delimited(*arguments, "(") else {
        return false;
    };
    let [name, selector] = inner.as_slice() else {
        return false;
    };
    matches!(name.utf8_text(source).ok(), Some("method" | "method_id"))
        && delimited(*selector, "(").is_some()
}

/// token tree の開き括弧が `open` なら、括弧の間の子 (コメントを含む)。
fn delimited<'tree>(tree: Node<'tree>, open: &str) -> Option<Vec<Node<'tree>>> {
    if tree.kind() != "token_tree" || tree.child(0)?.kind() != open {
        return None;
    }
    let mut cursor = tree.walk();
    let children = tree.children(&mut cursor).collect::<Vec<_>>();
    Some(children.get(1..children.len().checked_sub(1)?)?.to_vec())
}

/// ファイルの裸の `define_class!` が、objc2 の `define_class` だけを指すか。ファイル直下の `use` で
/// `objc2` から別名なしで 1 回だけ import し (`use objc2::{define_class, ..}`)、ほかの `use`・
/// `macro_rules!` で同じ名前を束縛せず、`#[macro_use]` も無い。
pub(super) fn define_class_imported(root: Node<'_>, source: &[u8]) -> bool {
    if names_identifier(root, source, &["macro_use"]) {
        return false;
    }
    let mut objc2_imports = 0usize;
    let mut other_bindings = 0usize;
    let _: std::ops::ControlFlow<()> = crate::engine::tree_walk::walk_preorder(
        root,
        crate::engine::tree_walk::Children::Named,
        |node, _| {
            match node.kind() {
                "use_declaration" => {
                    // コメントの中の綴り (`use std::fmt::{/* define_class */ Debug};`) は数えない。
                    if names_identifier(node, source, &["define_class"]) {
                        // 属性 (`#[cfg(any())]` など) を付けた `use` は、無効になり得るので数えない。
                        let from_objc2 = node
                            .parent()
                            .is_some_and(|parent| parent.kind() == "source_file")
                            && !preceded_by_attribute(node)
                            && node
                                .child_by_field_name("argument")
                                .is_some_and(|argument| imports_from_objc2(argument, source));
                        if from_objc2 {
                            objc2_imports += 1;
                        } else {
                            other_bindings += 1;
                        }
                    }
                    return std::ops::ControlFlow::Continue(crate::engine::tree_walk::Walk::Skip);
                }
                "macro_definition"
                    if node
                        .child_by_field_name("name")
                        .and_then(|name| name.utf8_text(source).ok())
                        .is_some_and(|name| name.trim_start_matches("r#") == "define_class") =>
                {
                    other_bindings += 1;
                }
                _ => {}
            }
            std::ops::ControlFlow::Continue(crate::engine::tree_walk::Walk::Descend)
        },
    );
    other_bindings == 0 && objc2_imports == 1
}

/// 項目の直前 (コメントを除く) に外側の属性があるか。
fn preceded_by_attribute(item: Node<'_>) -> bool {
    let mut previous = item.prev_named_sibling();
    while let Some(sibling) = previous {
        match sibling.kind() {
            "line_comment" | "block_comment" => previous = sibling.prev_named_sibling(),
            kind => return kind == "attribute_item",
        }
    }
    false
}

/// `use` の引数が `objc2` から `define_class` を別名なしで取り込むか (`objc2::define_class`・
/// `objc2::{define_class, ..}`)。
fn imports_from_objc2(argument: Node<'_>, source: &[u8]) -> bool {
    match argument.kind() {
        "scoped_identifier" => {
            argument
                .child_by_field_name("path")
                .and_then(|path| path.utf8_text(source).ok())
                .is_some_and(objc2_crate_path)
                && argument
                    .child_by_field_name("name")
                    .and_then(|name| name.utf8_text(source).ok())
                    == Some("define_class")
        }
        "scoped_use_list" => {
            argument
                .child_by_field_name("path")
                .and_then(|path| path.utf8_text(source).ok())
                .is_some_and(objc2_crate_path)
                && argument.child_by_field_name("list").is_some_and(|list| {
                    let mut cursor = list.walk();
                    list.named_children(&mut cursor).any(|item| {
                        item.kind() == "identifier"
                            && item.utf8_text(source).ok() == Some("define_class")
                    })
                })
        }
        _ => false,
    }
}

/// `objc2` の crate のパス (`objc2`・`::objc2`)。
fn objc2_crate_path(path: &str) -> bool {
    matches!(path, "objc2" | "::objc2")
}

/// ファイルに、名前の解決を変える属性があるか (読めない構文も真)。`#[macro_use]` は `.rs` でない
/// ファイルや木の外の `macro_rules!` を後のモジュールの textual scope に持ち込み、
/// `#[no_implicit_prelude]` は extern prelude を外して `objc2` をローカルの束縛だけで解く。属性の名前の
/// 識別子をどこでも数える (`cfg_attr` の中、マクロの本体・引数の token tree の中の
/// `#[no_implicit_prelude] $i` も。コメントと文字列の中の綴りは数えない)。
pub(crate) fn changes_name_resolution(root: Node<'_>, source: &[u8]) -> bool {
    root.has_error() || names_identifier(root, source, &["macro_use", "no_implicit_prelude"])
}

/// 木に、綴り (`r#` を外す) が `words` のどれかの識別子 (token tree の中を含む) があるか。
fn names_identifier(root: Node<'_>, source: &[u8], words: &[&str]) -> bool {
    crate::engine::tree_walk::any_node(root, crate::engine::tree_walk::Children::All, |node| {
        node.child_count() == 0
            && node.kind().ends_with("identifier")
            && node
                .utf8_text(source)
                .is_ok_and(|text| words.contains(&text.trim_start_matches("r#")))
    })
}

/// ファイルに、`define_class`・`objc2` を束縛し直し得る綴りがあるか (読めない構文も真)。
/// `define_class` は呼び出しの名前 (`define_class!`・`objc2::define_class!`) と、ファイル直下の
/// `use objc2::define_class`・`use objc2::{.., define_class}` だけ、`objc2` はパスの先頭 (`objc2::..`)
/// だけを許す。`use crate::shim as objc2;`・`mod objc2`・`pub use crate::body as define_class;`・
/// マクロの引数の `gen!(define_class)` などは真。木の全体で偽なら、裸の `define_class!` の名前と
/// `use objc2::..` の `objc2` を、別の定義へ付け替える束縛はどこにも書かれていない。
pub(super) fn rebinds_objc2_names(root: Node<'_>, source: &[u8]) -> bool {
    root.has_error()
        || crate::engine::tree_walk::any_node(
            root,
            crate::engine::tree_walk::Children::All,
            |node| {
                if node.child_count() != 0 || !node.kind().ends_with("identifier") {
                    return false;
                }
                let Ok(text) = node.utf8_text(source) else {
                    return true;
                };
                match text.trim_start_matches("r#") {
                    "define_class" => !define_class_name_allowed(node, source),
                    "objc2" => !objc2_path_head(node, source),
                    _ => false,
                }
            },
        )
}

/// `define_class` の識別子が、呼び出しの名前か、ファイル直下の `use objc2::..` が取り込む名前か。
fn define_class_name_allowed(node: Node<'_>, source: &[u8]) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        "macro_invocation" => parent.child_by_field_name("macro") == Some(node),
        "scoped_identifier" => {
            parent.child_by_field_name("name") == Some(node)
                && parent
                    .child_by_field_name("path")
                    .and_then(|path| path.utf8_text(source).ok())
                    .is_some_and(objc2_crate_path)
                && (top_level_objc2_use(parent, source)
                    || parent.parent().is_some_and(|invocation| {
                        invocation.kind() == "macro_invocation"
                            && invocation.child_by_field_name("macro") == Some(parent)
                    }))
        }
        "use_list" => parent
            .parent()
            .is_some_and(|list| top_level_objc2_use(list, source)),
        _ => false,
    }
}

/// `argument` が、ファイル直下の `use` の引数で、`objc2` から `define_class` を取り込むか。
fn top_level_objc2_use(argument: Node<'_>, source: &[u8]) -> bool {
    argument.parent().is_some_and(|declaration| {
        declaration.kind() == "use_declaration"
            && declaration.child_by_field_name("argument") == Some(argument)
            && declaration
                .parent()
                .is_some_and(|file| file.kind() == "source_file")
    }) && imports_from_objc2(argument, source)
}

/// `objc2` の識別子がパスの先頭 (`objc2::..`・`::objc2::..`) か。token tree の中は、裸の名前で呼んだ
/// `define_class!` の入力の中だけで、後ろが `::`、前が `::` でない (`::objc2` の `::` は前に名前が無い)。
fn objc2_path_head(node: Node<'_>, source: &[u8]) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        "scoped_identifier" | "scoped_type_identifier" | "scoped_use_list" => {
            parent.child_by_field_name("path") == Some(node)
                || (parent.child_by_field_name("name") == Some(node)
                    && parent.child_by_field_name("path").is_none())
        }
        "token_tree" => {
            let mut ancestor = parent.parent();
            while let Some(current) = ancestor {
                if current.kind() != "token_tree" {
                    break;
                }
                ancestor = current.parent();
            }
            let in_define_class = ancestor.is_some_and(|invocation| {
                invocation.kind() == "macro_invocation"
                    && invocation.child_by_field_name("macro").is_some_and(|name| {
                        name.kind() == "identifier"
                            && name.utf8_text(source).ok() == Some("define_class")
                    })
            });
            let global = |previous: Node<'_>| {
                previous.kind() == "::"
                    && previous_token(previous).is_none_or(|before| {
                        !matches!(
                            before.kind(),
                            "identifier" | "metavariable" | ">" | "self" | "super" | "crate"
                        )
                    })
            };
            in_define_class
                && next_token(node).is_some_and(|next| next.kind() == "::")
                && previous_token(node)
                    .is_none_or(|previous| previous.kind() != "::" || global(previous))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::parser;
    use crate::language::LangId;

    fn site_of(source: &str, needle: &str) -> MacroSite {
        let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
        let offset = source.find(needle).unwrap();
        let node = tree
            .root_node()
            .descendant_for_byte_range(offset, offset + needle.len())
            .unwrap();
        assert_eq!(node.utf8_text(source.as_bytes()).unwrap(), needle);
        macro_site(node, source.as_bytes())
    }

    #[test]
    fn opaque_attribute_scopes_cover_only_the_attributed_item_or_scope() {
        let source = concat!(
            "#[transform]\n#[allow(unused)]\n// attached\nfn f() { x.outer(); }\n",
            "fn g() { #![transform] x.inner(); }\n",
            "#[allow(unused)] fn h() { #[transform] x.statement(); x.plain(); }\n",
            "fn i() { x.sibling(); }\n",
        );
        let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
        let root = tree.root_node();
        assert!(!root.has_error());
        let index = OpaqueAttributeScopes::build(root, source.as_bytes());
        for (name, expected) in [
            ("outer", true),
            ("inner", true),
            ("statement", true),
            ("plain", false),
            ("sibling", false),
        ] {
            let start = source.find(name).unwrap();
            let node = root
                .descendant_for_byte_range(start, start + name.len())
                .unwrap();
            assert_eq!(index.contains(node), expected, "{name}");
            assert_eq!(
                macro_site(node, source.as_bytes()) != MacroSite::Plain,
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn macro_sites_follow_the_resolution_scope() {
        let body = "    impl C {\n        #[unsafe(method(go:))]\n        fn go(&self, e: u8) -> bool { guarded(|| { let k = translate(e); k }) }\n    }\n";
        let define = |body: &str| format!("define_class!(\n    struct C;\n{body});\n");
        for (source, expected) in [
            ("fn f() { translate(1); }".to_owned(), MacroSite::Plain),
            (
                "#[inline]\nfn f() { translate(1); }".to_owned(),
                MacroSite::Plain,
            ),
            (
                "#[some::attribute]\nfn f() { translate(1); }".to_owned(),
                MacroSite::Unknown,
            ),
            (
                "#[cfg_attr(test, some::attribute)]\nfn f() { translate(1); }".to_owned(),
                MacroSite::Unknown,
            ),
            // `#[test]` は `use other::export as test;` で付け替えられる。
            (
                "#[test]\nconst F: fn() = || { translate(1); };".to_owned(),
                MacroSite::Unknown,
            ),
            (
                "#![some_attribute]\nfn f() { translate(1); }".to_owned(),
                MacroSite::Unknown,
            ),
            (
                "fn f() { #[some::attribute] let x = translate(1); }".to_owned(),
                MacroSite::Unknown,
            ),
            (
                "macro_rules! m { () => { translate(1) }; }".to_owned(),
                MacroSite::RuleBody,
            ),
            (
                "macro_rules! m { ($($n:expr),*) => { $(translate($n);)* }; }".to_owned(),
                MacroSite::RuleBody,
            ),
            (
                "macro_rules! m { () => { S.translate(1) }; }".to_owned(),
                MacroSite::Unknown,
            ),
            (
                "macro_rules! m { () => { S { translate: 1 } }; }".to_owned(),
                MacroSite::Unknown,
            ),
            (
                "macro_rules! m { () => { S { translate } }; }".to_owned(),
                MacroSite::RuleBody,
            ),
            (
                "macro_rules! m { () => {{ let f: fn(u8) = translate; f(1) }}; }".to_owned(),
                MacroSite::RuleBody,
            ),
            (
                "macro_rules! m { () => { relay!(S.translate(1)) }; }".to_owned(),
                MacroSite::RuleBody,
            ),
            (
                "macro_rules! m { () => { crate::translate(1) }; }".to_owned(),
                MacroSite::RuleBody,
            ),
            (
                "macro_rules! m { () => { $crate /* origin */ :: api::translate(1) }; }".to_owned(),
                MacroSite::Unknown,
            ),
            (
                "macro_rules! m { ($module:ident) => { $module::translate(1) }; }".to_owned(),
                MacroSite::RuleBody,
            ),
            (
                "macro_rules! m { (translate(1)) => {}; }".to_owned(),
                MacroSite::Unknown,
            ),
            (
                "fn f() { pass!(translate(1)); }".to_owned(),
                MacroSite::Unknown,
            ),
            (define(body), MacroSite::DefineClassBody),
            (
                define(&body.replace("method(go:)", "method_id(go:)")),
                MacroSite::DefineClassBody,
            ),
            // 先頭の `objc2` は付け替えられる。
            (format!("objc2::{}", define(body)), MacroSite::Unknown),
            (format!("other::{}", define(body)), MacroSite::Unknown),
            (
                format!("#[some::attribute]\n{}", define(body)),
                MacroSite::Unknown,
            ),
            // メソッドの本体の中のほかのマクロの引数 (コメントを挟んでも) と、メソッドでない位置。
            (
                define(
                    "    impl C {\n        fn go(&self) { println!(\"{}\", translate(1)); }\n    }\n",
                ),
                MacroSite::Unknown,
            ),
            (
                define(
                    "    impl C {\n        fn go(&self) { install! /* note */ (translate(1)); }\n    }\n",
                ),
                MacroSite::Unknown,
            ),
            (
                define("    impl C {\n        const X: u8 = { translate(1) };\n    }\n"),
                MacroSite::Unknown,
            ),
            (
                "define_class!(\n    struct C(translate);\n);\n".to_owned(),
                MacroSite::Unknown,
            ),
            // 本体の中の属性を付けた項目と、本体の内側の属性。
            (
                define(
                    "    impl C {\n        fn go(&self) { #[emit] fn template() { translate(1); } }\n    }\n",
                ),
                MacroSite::Unknown,
            ),
            (
                define("    impl C {\n        fn go(&self) { #![emit] translate(1); }\n    }\n"),
                MacroSite::Unknown,
            ),
            // メソッドと `impl` の、objc2 の印でない属性は、生成するメソッド・`impl` に付く。
            (
                define(&body.replace(
                    "#[unsafe(method(go:))]",
                    "#[unsafe(method(go:))]\n        #[emit]",
                )),
                MacroSite::Unknown,
            ),
            (
                define(&body.replace("#[unsafe(method(go:))]", "#[inline]")),
                MacroSite::Unknown,
            ),
            (
                define(&body.replace("    impl C {", "    #[emit]\n    impl C {")),
                MacroSite::Unknown,
            ),
            // 戻り値の型の const 引数の波括弧で、メソッドの属性を見失わない。
            (
                define(&body.replace(
                    "#[unsafe(method(go:))]\n        fn go(&self, e: u8) -> bool",
                    "#[unsafe(method(go:))]\n        #[emit]\n        fn go(&self, e: u8) -> R<{1}, fn()>",
                )),
                MacroSite::Unknown,
            ),
            (
                define(&body.replace(
                    "#[unsafe(method(go:))]\n        fn go(&self, e: u8) -> bool",
                    "#[unsafe(method(go:))]\n        #[emit]\n        fn go(&self, e: u8) -> R<ty!{}, fn()>",
                )),
                MacroSite::Unknown,
            ),
            // 本体の中の `macro_rules!` の本体は、展開した場所で名前を解決する。
            (
                define("    impl C {\n        fn go(&self) {\n            macro_rules! make {\n                ($($attr:tt)*) => { $($attr)* macro_rules! exported { () => { translate(1) }; } };\n            }\n            make!(#[macro_export]);\n        }\n    }\n"),
                MacroSite::Unknown,
            ),
            // ファイル直下でない呼び出し (中の `mod` には、ファイル直下の `use` が届かない)。
            (
                format!("mod inner {{\n{}}}\n", define(body)),
                MacroSite::Unknown,
            ),
            // 戻り値の型の位置のマクロの引数 (`-> ret! { .. }`) と、`impl` の見出しのマクロの引数。
            (
                define("    impl C {\n        #[unsafe(method_id(go))]\n        fn go(&self) -> ret! { translate(1) } { None }\n    }\n"),
                MacroSite::Unknown,
            ),
            (
                define("    impl C {\n        #[unsafe(method_id(go))]\n        fn go(&self) -> ret!(u8) { translate(1) }\n    }\n"),
                MacroSite::Unknown,
            ),
            (
                define("    impl ret! { fn go(&self) { translate(1) } } {\n    }\n"),
                MacroSite::Unknown,
            ),
        ] {
            assert_eq!(site_of(&source, "translate"), expected, "{source}");
        }
        // 対照: 属性の無いメソッドと、別のメソッドの本体の属性は数えない。
        let other = "    impl C {\n        fn other(&self) { #[allow(unused)] let x = 1; }\n        fn go(&self) { translate(1); }\n    }\n";
        assert_eq!(
            site_of(&define(other), "translate"),
            MacroSite::DefineClassBody
        );
    }

    #[test]
    fn define_class_must_come_from_objc2() {
        for (source, expected) in [
            (
                "use objc2::{define_class, msg_send};\ndefine_class!();\n",
                true,
            ),
            ("use objc2::define_class;\ndefine_class!();\n", true),
            ("objc2::define_class!();\n", false),
            ("define_class!();\n", false),
            ("use objc2::define_class as dc;\ndefine_class!();\n", false),
            ("use other::define_class;\ndefine_class!();\n", false),
            (
                "use objc2::define_class;\nmacro_rules! define_class { () => {}; }\ndefine_class!();\n",
                false,
            ),
            (
                "#[macro_use]\nextern crate other;\nuse objc2::define_class;\ndefine_class!();\n",
                false,
            ),
            (
                "use ::objc2::{define_class, msg_send};\ndefine_class!();\n",
                true,
            ),
            // 属性を付けた `use` は無効になり得る。
            (
                "#[cfg(any())]\nuse objc2::define_class;\ndefine_class!();\n",
                false,
            ),
            // マクロの本体の `#[macro_use]` は数え、コメントの綴りは数えない。
            (
                "macro_rules! m { () => { #[macro_use] extern crate other; }; }\nuse objc2::define_class;\ndefine_class!();\n",
                false,
            ),
            (
                "// macro_use は不要\nuse objc2::define_class;\ndefine_class!();\n",
                true,
            ),
            (
                "use std::fmt::{/* define_class */ Debug};\nuse objc2::define_class;\ndefine_class!();\n",
                true,
            ),
        ] {
            let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
            assert_eq!(
                define_class_imported(tree.root_node(), source.as_bytes()),
                expected,
                "{source}"
            );
        }
    }

    #[test]
    fn name_resolution_attributes_are_found() {
        for (source, expected) in [
            (
                "// macro_use is not used here\nconst S: &str = \"no_implicit_prelude\";\n",
                false,
            ),
            ("#[macro_use]\nextern crate log;\n", true),
            ("#![no_implicit_prelude]\n", true),
            ("#[no_implicit_prelude]\nmod native;\n", true),
            ("#[cfg_attr(unix, macro_use)]\nmod macros;\n", true),
            // マクロの本体と引数の token tree の中の属性。
            (
                "macro_rules! off { ($i:item) => { #[no_implicit_prelude] $i }; }\n",
                true,
            ),
            ("wrap!(#[macro_use] mod macros;);\n", true),
            // 属性の値の文字列は数えない。
            ("#[doc = \"no_implicit_prelude\"]\nfn f() {}\n", false),
        ] {
            let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
            assert_eq!(
                changes_name_resolution(tree.root_node(), source.as_bytes()),
                expected,
                "{source}"
            );
        }
    }

    #[test]
    fn objc2_names_must_not_be_rebound_anywhere() {
        for (source, expected) in [
            (
                "use objc2::{define_class, msg_send, rc::Retained};\nuse objc2::runtime::AnyObject;\nuse ::objc2::rc::Weak;\ndefine_class!(impl C { fn go(&self) -> objc2::rc::Retained<u8> { ::objc2::x() } });\nobjc2::define_class!();\nfn f(x: objc2::rc::Retained<u8>) { objc2::msg_send![x, go]; }\n",
                false,
            ),
            // ほかのマクロの引数の `objc2::tag` は、マクロが `use .. as objc2` に組み立て得る。
            ("bind!(objc2::tag);\n", true),
            ("m!(objc2::x);\n", true),
            ("use crate::shim as objc2;\n", true),
            ("use crate::shim as r#objc2;\n", true),
            ("mod objc2;\n", true),
            ("mod objc2 { pub use crate::body as define_class; }\n", true),
            ("pub use crate::body as define_class;\n", true),
            ("use crate::compat::objc2;\n", true),
            ("use crate::{objc2, other};\n", true),
            ("extern crate shim as objc2;\n", true),
            ("struct objc2;\n", true),
            ("fn f() { crate::objc2::define_class!(); }\n", true),
            ("gen!(define_class);\n", true),
            ("gen!(use crate::shim as objc2;);\n", true),
            ("macro_rules! define_class { () => {}; }\n", true),
            ("fn f() { use objc2::define_class; }\n", true),
            ("use other::define_class;\n", true),
            // 文字列とコメントは数えない。
            (
                "// objc2 define_class\nconst S: &str = \"objc2 define_class\";\n",
                false,
            ),
        ] {
            let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
            assert_eq!(
                rebinds_objc2_names(tree.root_node(), source.as_bytes()),
                expected,
                "{source}"
            );
        }
    }
}
