//! Rust の変更ファイルで、旧新の公開シンボルを宣言の場所で組にする (Issue #282)。
//!
//! 公開シンボルの qualname は、inline module の中の項目でも裸の名前 (`run`・`Foo.run`) なので、
//! 同じファイルの別々の inline module (`pub mod a { pub fn run() }` と `pub mod b { .. }`) に同じ
//! 名前の項目があると、名前だけでは旧新を組にできない。宣言を囲む inline module の経路・impl の
//! 対象の型 (メソッドなど)・qualname・種類をキーにして組にし、追加・削除・変更をキーごとに決める。
//!
//! 同じキーの宣言が複数ある (`#[cfg]` の分岐など) ときは、旧新の宣言を 1 対 1 にできない。
//! どちらの側も条件付きの分岐だけなら、signature がすべて同じとき契約は同じとみなし、各側の
//! signature がそれぞれ 1 種類ならその 2 つを比べる。そうでなければ cfg の条件と signature の
//! 組が旧新で同じときだけ変わっていないとみなし、ほかは組にできない変更として blocking に残す。
//! cfg の条件の字面は、この比較にだけ使い、組のキーにはしない。

use super::*;

/// Rust の公開シンボルの宣言の文脈。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RustDeclarationContext {
    /// 宣言を囲む inline module の名前の並び (`a::b`)。ファイルの module の直下なら空。
    pub(crate) inline_module: String,
    /// 宣言を囲む最も内側の impl の対象の型のトークン (`impl a::Foo` の `a::Foo`)。impl の
    /// 外なら空。同じ module の別々の型の impl にある同じ名前のメソッドを区別する。
    pub(crate) owner: Vec<String>,
    /// 宣言と、それを囲む項目 (impl・trait・inline module・extern ブロック) に付いた `cfg`・
    /// `cfg_attr` 属性のトークンの並び (外側から)。同じキーの宣言が複数あるときの比較にだけ使う。
    pub(crate) conditions: Vec<String>,
    /// 宣言の始まりの位置 (行, 列。0 始まり)。
    pub(crate) start: (usize, usize),
}

/// 変更ファイルの旧新の公開シンボルの宣言の文脈 (それぞれ `old_syms`・`new_syms` と同じ順)。
#[derive(Debug)]
pub(crate) struct RustDeclarationPair {
    pub(crate) old: Vec<RustDeclarationContext>,
    pub(crate) new: Vec<RustDeclarationContext>,
    /// 新しい版で、関数とメソッドの宣言 (非公開を含む) に 2 回以上現れる名前。同じファイルの
    /// 中の呼び出しは名前で数えるので、これらの名前の呼び出しはどの宣言のものか決められない。
    pub(crate) new_shared_callables: HashSet<String>,
}

impl RustDeclarationPair {
    /// 旧い版の `index` 番目の宣言を囲む inline module の経路 (ファイルの module の直下なら None)。
    pub(crate) fn old_inline_module(&self, index: usize) -> Option<String> {
        inline_module_of(self.old.get(index)?)
    }

    /// 新しい版の `index` 番目の宣言を囲む inline module の経路。
    pub(crate) fn new_inline_module(&self, index: usize) -> Option<String> {
        inline_module_of(self.new.get(index)?)
    }
}

/// 宣言を囲む inline module の経路 (ファイルの module の直下なら None)。
pub(crate) fn inline_module_of(declaration: &RustDeclarationContext) -> Option<String> {
    (!declaration.inline_module.is_empty()).then(|| declaration.inline_module.clone())
}

/// 属性 (`attribute_item`・`inner_attribute_item`) が `cfg` か `cfg_attr` か。
fn is_condition_attribute(attribute: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    let mut cursor = attribute.walk();
    let Some(inner) = attribute
        .named_children(&mut cursor)
        .find(|child| child.kind() == "attribute")
    else {
        return false;
    };
    let mut inner_cursor = inner.walk();
    inner
        .named_children(&mut inner_cursor)
        .next()
        .and_then(|path| path.utf8_text(source).ok())
        .is_some_and(|path| matches!(path, "cfg" | "cfg_attr"))
}

/// `item` の直前に並ぶ外側の属性のうち、`cfg`・`cfg_attr` のトークンの並び (書いた順)。
fn preceding_conditions(item: tree_sitter::Node<'_>, source: &[u8]) -> Vec<String> {
    let mut attributes = Vec::new();
    let mut previous = item.prev_named_sibling();
    while let Some(sibling) = previous {
        match sibling.kind() {
            "attribute_item" => {
                if is_condition_attribute(sibling, source) {
                    attributes.push(sibling);
                }
            }
            "line_comment" | "block_comment" => {}
            _ => break,
        }
        previous = sibling.prev_named_sibling();
    }
    let mut tokens = Vec::new();
    for attribute in attributes.into_iter().rev() {
        push_tokens(attribute, source, &mut tokens);
    }
    tokens
}

/// 中身 (`declaration_list`) の内側の属性のうち、`cfg`・`cfg_attr` のトークンの並び。
fn inner_conditions(body: tree_sitter::Node<'_>, source: &[u8]) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() == "inner_attribute_item" && is_condition_attribute(child, source) {
            push_tokens(child, source, &mut tokens);
        }
    }
    tokens
}

/// シンボル `sym` の宣言の文脈。宣言のノードを引けなければ、直下で条件の無い宣言とみなす
/// (位置だけを持つ)。
pub(crate) fn rust_declaration_context(
    sym: &Symbol,
    root: tree_sitter::Node<'_>,
    source: &[u8],
) -> RustDeclarationContext {
    let start = tree_sitter::Point {
        row: sym.range.start.line,
        column: sym.range.start.column,
    };
    let end = tree_sitter::Point {
        row: sym.range.end.line,
        column: sym.range.end.column,
    };
    let mut context = RustDeclarationContext {
        start: (start.row, start.column),
        ..RustDeclarationContext::default()
    };
    let Some(mut item) = root.descendant_for_point_range(start, end) else {
        return context;
    };
    // 宣言の項目 (ファイルか中身の直下に並ぶノード) まで上がる。enum の variant などは囲む項目。
    while let Some(parent) = item.parent() {
        if matches!(parent.kind(), "source_file" | "declaration_list") {
            break;
        }
        item = parent;
    }
    // 外側から並べるため、内側から集めて最後に逆にする。
    let mut modules: Vec<String> = Vec::new();
    let mut conditions: Vec<Vec<String>> = vec![preceding_conditions(item, source)];
    let mut node = item;
    while let Some(body) = node.parent() {
        if body.kind() != "declaration_list" {
            break;
        }
        let Some(container) = body.parent() else {
            break;
        };
        conditions.push(inner_conditions(body, source));
        conditions.push(preceding_conditions(container, source));
        if container.kind() == "impl_item"
            && context.owner.is_empty()
            && let Some(owner) = container.child_by_field_name("type")
        {
            push_tokens(owner, source, &mut context.owner);
        }
        if container.kind() == "mod_item"
            && let Some(name) = container
                .child_by_field_name("name")
                .and_then(|name| name.utf8_text(source).ok())
        {
            modules.push(name.to_owned());
        }
        node = container;
    }
    modules.reverse();
    conditions.reverse();
    context.inline_module = modules.join("::");
    context.conditions = conditions.into_iter().flatten().collect();
    context
}

/// Rust の変更ファイルの旧新の公開シンボルを、宣言の場所 (inline module の経路・qualname・
/// 種類) で組にした結果。
pub(crate) struct RustPairing {
    /// 旧いシンボルごとに、新しい版に同じキーの宣言が無い (削除)。
    pub(crate) removed: Vec<bool>,
    /// 新しいシンボルごとに、旧い版に同じキーの宣言が無い (追加)。
    pub(crate) added: Vec<bool>,
    /// 旧新が 1 つずつのキーの組 (旧の添字, 新の添字)。
    pub(crate) pairs: Vec<(usize, usize)>,
    /// 同じキーの宣言が複数あり、旧新で cfg の条件と signature の対応が変わったキーの、旧と新の
    /// 添字の並び。どの宣言どうしを比べればよいか決められない。
    pub(crate) ambiguous: Vec<(Vec<usize>, Vec<usize>)>,
    /// 旧か新に、同じ qualname の宣言が 2 つ以上ある名前。名前で引く判定は別の宣言に当たり得る。
    shared_names: HashSet<String>,
}

impl RustPairing {
    /// 旧新のシンボルと宣言の文脈 (同じ順) から組を作る。数が合わなければ None。
    pub(crate) fn build(
        old: &[(String, String, String)],
        old_declarations: &[RustDeclarationContext],
        new: &[(String, String, String)],
        new_declarations: &[RustDeclarationContext],
    ) -> Option<Self> {
        if old.len() != old_declarations.len() || new.len() != new_declarations.len() {
            return None;
        }
        type Key<'a> = (&'a str, &'a [String], &'a str, &'a str);
        let mut groups: std::collections::BTreeMap<Key<'_>, (Vec<usize>, Vec<usize>)> =
            std::collections::BTreeMap::new();
        let mut counts: std::collections::HashMap<&str, (usize, usize)> =
            std::collections::HashMap::new();
        for (i, ((name, kind, _), declaration)) in old.iter().zip(old_declarations).enumerate() {
            let key = (
                declaration.inline_module.as_str(),
                declaration.owner.as_slice(),
                name.as_str(),
                kind.as_str(),
            );
            groups.entry(key).or_default().0.push(i);
            counts.entry(name.as_str()).or_default().0 += 1;
        }
        for (j, ((name, kind, _), declaration)) in new.iter().zip(new_declarations).enumerate() {
            let key = (
                declaration.inline_module.as_str(),
                declaration.owner.as_slice(),
                name.as_str(),
                kind.as_str(),
            );
            groups.entry(key).or_default().1.push(j);
            counts.entry(name.as_str()).or_default().1 += 1;
        }
        let mut pairing = Self {
            removed: vec![false; old.len()],
            added: vec![false; new.len()],
            pairs: Vec::new(),
            ambiguous: Vec::new(),
            shared_names: counts
                .into_iter()
                .filter(|(_, (old, new))| *old > 1 || *new > 1)
                .map(|(name, _)| name.to_owned())
                .collect(),
        };
        for (olds, news) in groups.into_values() {
            match (olds.as_slice(), news.as_slice()) {
                ([], _) => news.iter().for_each(|&j| pairing.added[j] = true),
                (_, []) => olds.iter().for_each(|&i| pairing.removed[i] = true),
                ([i], [j]) => pairing.pairs.push((*i, *j)),
                _ => {
                    let old_signatures: HashSet<&str> =
                        olds.iter().map(|&i| old[i].2.as_str()).collect();
                    let new_signatures: HashSet<&str> =
                        news.iter().map(|&j| new[j].2.as_str()).collect();
                    // どちらの側も条件付きの分岐だけ (1 つなら条件は問わない) なら、分岐をまとめた・
                    // 分けた変更として扱える。signature がすべて同じなら利用者から見た契約は
                    // 変わらない (どの設定で使えるかの変化は比べない)。各側の signature がそれぞれ
                    // 1 種類なら、どの設定でも契約はその 1 つなので、その 2 つを比べる。
                    if only_conditional_variants(&olds, old_declarations)
                        && only_conditional_variants(&news, new_declarations)
                        && old_signatures.len() == 1
                        && new_signatures.len() == 1
                    {
                        if old_signatures != new_signatures {
                            pairing.pairs.push((olds[0], news[0]));
                        }
                        continue;
                    }
                    if condition_signatures(&olds, old, old_declarations)
                        != condition_signatures(&news, new, new_declarations)
                    {
                        pairing.ambiguous.push((olds, news));
                    }
                }
            }
        }
        // 出力の順を新しい版の宣言の順にそろえる。
        pairing.pairs.sort_by_key(|&(_, j)| j);
        Some(pairing)
    }

    /// 旧か新に、同じ qualname `name` の宣言が 2 つ以上あるか。
    pub(crate) fn name_is_shared(&self, name: &str) -> bool {
        self.shared_names.contains(name)
    }
}

/// 同じキーの宣言 `indices` が、1 つか、どれも cfg の条件の付いた分岐か。
fn only_conditional_variants(indices: &[usize], declarations: &[RustDeclarationContext]) -> bool {
    indices.len() <= 1
        || indices
            .iter()
            .all(|&k| !declarations[k].conditions.is_empty())
}

/// 同じキーの宣言 `indices` の、cfg の条件と signature の組 (並べ替え済み)。
fn condition_signatures<'a>(
    indices: &[usize],
    symbols: &'a [(String, String, String)],
    declarations: &'a [RustDeclarationContext],
) -> Vec<(&'a [String], &'a str)> {
    let mut entries: Vec<(&[String], &str)> = indices
        .iter()
        .map(|&k| (declarations[k].conditions.as_slice(), symbols[k].2.as_str()))
        .collect();
    entries.sort();
    entries
}

/// Rust の宣言を宣言の場所で組にして比べるときの、名前で引く判定に代わる情報。
#[derive(Clone, Copy, Debug)]
pub(crate) struct RustDeclarationSite {
    /// 旧新の宣言の始まりの位置 (行, 列)。
    pub(crate) old_start: (usize, usize),
    pub(crate) new_start: (usize, usize),
    /// 同じ qualname の宣言がファイルに複数ある。名前で引く判定 (同じファイルの中だけで使う、
    /// 呼び出し側の追随、型別名の同値) は別の宣言に当たり得るので使わない。
    pub(crate) name_shared: bool,
    /// 新しい版で、名前の末尾 (`Foo.run` なら `run`) と同じ名前の関数・メソッドの宣言 (非公開を
    /// 含む) が複数ある。同じファイルの中の呼び出しがどの宣言のものか決められないので、同じ
    /// ファイルの中だけで使う証明にしない。
    pub(crate) callee_shared: bool,
}

/// Rust のソース `source` の、名前が `name` の関数・メソッドの宣言 (非公開・入れ子を含む) の数。
/// 構文エラーなら None。
pub(crate) fn callable_declarations(source: &[u8], name: &str) -> Option<usize> {
    let tree = parser::parse_source(source, crate::language::LangId::Rust).ok()?;
    if tree.root_node().has_error() {
        return None;
    }
    let mut count = 0;
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if matches!(node.kind(), "function_item" | "function_signature_item")
            && node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
                .is_some_and(|declared| declared.strip_prefix("r#").unwrap_or(declared) == name)
        {
            count += 1;
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    Some(count)
}

/// Rust のソース `source` の木 `root` で、`start` (行, 列) に始まる種類 `kind` の項目。
fn item_at<'tree>(
    root: tree_sitter::Node<'tree>,
    start: (usize, usize),
    kind: &str,
) -> Option<tree_sitter::Node<'tree>> {
    let point = tree_sitter::Point {
        row: start.0,
        column: start.1,
    };
    let mut node = root.descendant_for_point_range(point, point)?;
    loop {
        if node.kind() == kind && node.start_position() == point {
            return Some(node);
        }
        node = node.parent()?;
    }
}

/// `source` の `start` に始まる構造体の形。
pub(crate) fn struct_shape_at(
    source: &[u8],
    start: (usize, usize),
) -> Option<super::rust_struct_api::RustStructShape> {
    let tree = parser::parse_source(source, crate::language::LangId::Rust).ok()?;
    super::rust_struct_api::RustStructShape::from_node(
        item_at(tree.root_node(), start, "struct_item")?,
        source,
    )
}

/// `source` の `start` に始まる enum の形。
pub(crate) fn enum_shape_at(
    source: &[u8],
    start: (usize, usize),
) -> Option<super::rust_enum_api::RustEnumShape> {
    let tree = parser::parse_source(source, crate::language::LangId::Rust).ok()?;
    super::rust_enum_api::RustEnumShape::from_node(
        item_at(tree.root_node(), start, "enum_item")?,
        source,
    )
}

/// `source` の `start` に始まる union の形。
pub(crate) fn union_shape_at(
    source: &[u8],
    start: (usize, usize),
) -> Option<super::rust_union_api::RustUnionShape> {
    let tree = parser::parse_source(source, crate::language::LangId::Rust).ok()?;
    super::rust_union_api::RustUnionShape::from_node(
        item_at(tree.root_node(), start, "union_item")?,
        source,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `source` の公開シンボルと宣言の文脈。
    fn declarations(source: &str) -> Vec<((String, String, String), RustDeclarationContext)> {
        let lang = crate::language::LangId::Rust;
        let tree = parser::parse_source(source.as_bytes(), lang).expect("parse");
        let root = tree.root_node();
        let symbols =
            crate::engine::symbols::extract_symbols(root, source.as_bytes(), lang).expect("syms");
        let facts = filter_exported_file_facts(
            &symbols,
            root,
            source.as_bytes(),
            lang,
            true,
            false,
            None,
            false,
        );
        facts
            .symbols
            .into_iter()
            .zip(facts.rust_declarations.expect("rust declarations"))
            .collect()
    }

    #[test]
    fn declaration_contexts_name_inline_modules_and_conditions() {
        let found = declarations(
            "pub fn run() {}\npub mod a {\n    #[cfg(unix)]\n    pub fn run(x: u8) {}\n    pub mod b {\n        pub struct Note;\n        impl Note {\n            pub fn run(&self) {}\n        }\n    }\n}\n",
        );
        let context = |name: &str, kind: &str| {
            found
                .iter()
                .filter(|((n, k, _), _)| n == name && k == kind)
                .map(|(_, context)| context.clone())
                .collect::<Vec<_>>()
        };
        let runs = context("run", "function");
        assert_eq!(runs.len(), 2, "{found:?}");
        assert!(runs.iter().any(|c| c.inline_module.is_empty()), "{runs:?}");
        let in_a = runs
            .iter()
            .find(|c| c.inline_module == "a")
            .expect("a::run");
        assert!(
            in_a.conditions.iter().any(|t| t.ends_with("unix")),
            "{in_a:?}"
        );
        let note = context("Note", "struct");
        assert_eq!(note.len(), 1, "{found:?}");
        assert_eq!(note[0].inline_module, "a::b");
        assert!(note[0].conditions.is_empty(), "{note:?}");
        let method = found
            .iter()
            .find(|((n, _, _), _)| n.ends_with("run") && n.contains('.'))
            .expect("method");
        assert_eq!(method.1.inline_module, "a::b", "{found:?}");
    }

    /// 旧新のソースを組にした結果。
    fn pairing(old: &str, new: &str) -> (RustPairing, Vec<(String, String, String)>) {
        let old = declarations(old);
        let new = declarations(new);
        let (old_symbols, old_declarations): (Vec<_>, Vec<_>) = old.into_iter().unzip();
        let (new_symbols, new_declarations): (Vec<_>, Vec<_>) = new.into_iter().unzip();
        (
            RustPairing::build(
                &old_symbols,
                &old_declarations,
                &new_symbols,
                &new_declarations,
            )
            .expect("pairing"),
            new_symbols,
        )
    }

    #[test]
    fn pairs_same_named_items_by_inline_module() {
        let (paired, _) = pairing(
            "pub mod a {\n    pub fn run(x: u8) {}\n}\npub mod b {\n    pub fn run() {}\n}\n",
            "pub mod a {\n    pub fn run(x: u8, y: u8) {}\n}\npub mod b {\n    pub fn run() {}\n}\n",
        );
        assert_eq!(paired.pairs.len(), 2);
        assert!(paired.removed.iter().all(|r| !r) && paired.added.iter().all(|a| !a));
        assert!(paired.ambiguous.is_empty());
        assert!(paired.name_is_shared("run"));
        // 同じ signature の a::run を消し b::run を残すのは削除
        let (paired, _) = pairing(
            "pub mod a {\n    pub fn run() {}\n}\npub mod b {\n    pub fn run() {}\n}\n",
            "pub mod b {\n    pub fn run() {}\n}\n",
        );
        assert_eq!(paired.removed.iter().filter(|r| **r).count(), 1);
        assert_eq!(paired.pairs.len(), 1);
    }

    #[test]
    fn same_key_groups_compare_conditions_with_signatures() {
        let unix_u8 =
            "#[cfg(unix)]\npub fn run(x: u8) {}\n#[cfg(windows)]\npub fn run(x: u16) {}\n";
        // 並べ替えは変わっていない
        let (paired, _) = pairing(
            unix_u8,
            "#[cfg(windows)]\npub fn run(x: u16) {}\n#[cfg(unix)]\npub fn run(x: u8) {}\n",
        );
        assert!(paired.ambiguous.is_empty());
        // 条件の間で signature を入れ替えると、組にできない変更
        let (paired, _) = pairing(
            unix_u8,
            "#[cfg(unix)]\npub fn run(x: u16) {}\n#[cfg(windows)]\npub fn run(x: u8) {}\n",
        );
        assert_eq!(paired.ambiguous.len(), 1);
        // 同じ signature の分岐を 1 つにまとめるのは契約が同じ
        let (paired, _) = pairing(
            "#[cfg(unix)]\npub fn run(x: u8) {}\n#[cfg(windows)]\npub fn run(x: u8) {}\n",
            "pub fn run(x: u8) {}\n",
        );
        assert!(paired.ambiguous.is_empty() && paired.pairs.is_empty());
        assert!(paired.removed.iter().all(|r| !r) && paired.added.iter().all(|a| !a));
    }

    #[test]
    fn shapes_are_read_at_the_declaration() {
        let source = b"pub mod a {\n    pub struct Note {\n        pub text: u8,\n    }\n}\npub mod b {\n    pub struct Note {\n        pub tag: u8,\n    }\n}\n";
        let first = struct_shape_at(source, (1, 4)).expect("a::Note");
        let second = struct_shape_at(source, (6, 4)).expect("b::Note");
        assert_ne!(
            super::super::rust_struct_api::classify(&first, &second),
            super::super::rust_type_api::ShapeChange::Unchanged
        );
        assert!(struct_shape_at(source, (0, 0)).is_none());
    }
}
