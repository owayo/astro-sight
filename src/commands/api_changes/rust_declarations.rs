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
    /// 宣言を囲む最も内側の impl・trait の見出し (`impl<T: Copy> S<T> where ..` の、中身の前までの
    /// トークン)。同じキーの分岐を比べるとき、残った分岐の契約に含める (Issue #291)。
    pub(crate) container_header: Vec<String>,
    /// 宣言の有る条件 (Issue #291)。宣言自身の外側の属性と本体の内側の属性 (関数・trait)、囲む
    /// 項目 (impl・trait・inline module・extern ブロック) の外側の属性と中身の内側の属性、ファイルの
    /// 先頭の内側の属性のうち、`cfg` と、`cfg` だけに絞った `cfg_attr` (外側から)。読めない属性が
    /// あれば None。別のファイルの親 module (`#[cfg(x)] mod m;`) の条件は含まない。
    pub(crate) availability: Option<Vec<AvailabilityAttr>>,
    /// `availability` に入れる属性 (読めないものも) の `cfg`・`cfg_attr` の生のトークン (外側から)。
    /// 読めない属性があるときの字面の比較に使う。
    pub(crate) availability_raw: Vec<String>,
    /// 宣言を囲む inline module が全て `pub` (制限なし) か (Issue #305)。ファイルの module の直下なら真。
    pub(crate) inline_public: bool,
}

/// 宣言の有る条件の属性 1 つ (`cfg(..)` か、`cfg` だけに絞った `cfg_attr(..)`)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AvailabilityAttr {
    /// 比べる鍵 (`rust_cfg_predicate::Atoms::presence` に渡す)。
    pub(crate) key: Vec<String>,
    /// 表示 (`#[` と `]` を除く)。
    pub(crate) shown: String,
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
        // `r#cfg` も `cfg` (raw identifier の `r#` は名前に含まれない)。
        .is_some_and(|path| matches!(path.strip_prefix("r#").unwrap_or(path), "cfg" | "cfg_attr"))
}

/// `item` の直前に並ぶ外側の属性のうち、`cfg`・`cfg_attr` のトークンの並び (書いた順)。
pub(crate) fn preceding_conditions(item: tree_sitter::Node<'_>, source: &[u8]) -> Vec<String> {
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

/// `item` の直前に並ぶ外側の属性 (`attribute_item`) のノード (内側から。コメントは飛ばす)。
fn preceding_attribute_items(item: tree_sitter::Node<'_>) -> Vec<tree_sitter::Node<'_>> {
    let mut attributes = Vec::new();
    let mut previous = item.prev_named_sibling();
    while let Some(sibling) = previous {
        match sibling.kind() {
            "attribute_item" => attributes.push(sibling),
            "line_comment" | "block_comment" => {}
            _ => break,
        }
        previous = sibling.prev_named_sibling();
    }
    attributes
}

/// 中身 (`declaration_list`・`block`・`source_file`) の内側の属性 (`inner_attribute_item`) のノード。内側の
/// 属性は中身の先頭 (ファイルなら shebang の後) にしか書けない (項目の後ろは rustc が拒む) ので、項目が来たら
/// 止める (項目ごとにファイルの全ての項目を読まない)。
fn inner_attribute_items(body: tree_sitter::Node<'_>) -> Vec<tree_sitter::Node<'_>> {
    let mut cursor = body.walk();
    body.named_children(&mut cursor)
        .filter(|child| !matches!(child.kind(), "line_comment" | "block_comment" | "shebang"))
        .take_while(|child| child.kind() == "inner_attribute_item")
        .collect()
}

/// 属性のノードのうち、有る条件に効くもの (`cfg` と、`cfg` だけに絞った `cfg_attr`) を `out` に足す。
/// 読めない属性があれば `out` を None にする。
fn push_availability(
    attributes: &[tree_sitter::Node<'_>],
    source: &[u8],
    out: &mut Option<Vec<AvailabilityAttr>>,
) {
    for attribute in attributes {
        let Some(found) = out.as_mut() else {
            return;
        };
        match super::rust_type_api::project_attribute(*attribute, source, &["cfg"]) {
            Some(Some(attr)) => found.push(AvailabilityAttr {
                key: attr.key,
                shown: attr.shown,
            }),
            Some(None) => {}
            None => *out = None,
        }
    }
}

/// 宣言の項目 `item` を囲む項目 (中身が `declaration_list` の impl・trait・inline module・extern
/// ブロック) の中身の内側の属性と外側の属性、ファイルの先頭の内側の属性のノード (内側から)。
fn enclosing_attribute_items(item: tree_sitter::Node<'_>) -> Vec<tree_sitter::Node<'_>> {
    let mut attributes = Vec::new();
    let mut node = item;
    while let Some(body) = node.parent() {
        if body.kind() != "declaration_list" {
            break;
        }
        let Some(container) = body.parent() else {
            break;
        };
        attributes.extend(inner_attribute_items(body).into_iter().rev());
        attributes.extend(preceding_attribute_items(container));
        node = container;
    }
    if let Some(file) = node.parent()
        && file.kind() == "source_file"
    {
        attributes.extend(inner_attribute_items(file).into_iter().rev());
    }
    attributes
}

/// 属性のノードのうち、有る条件に効く部分のトークン (並びの順): `cfg` と、`cfg_attr` の `cfg` に
/// 絞った鍵。読めない `cfg_attr` は属性全体。`cfg` を含まない `cfg_attr(x, doc = "..")` や、
/// `cfg_attr` の中のほかの属性は入れない。
fn condition_tokens(attributes: &[tree_sitter::Node<'_>], source: &[u8]) -> Vec<String> {
    let mut tokens = Vec::new();
    for attribute in attributes {
        if !is_condition_attribute(*attribute, source) {
            continue;
        }
        match super::rust_type_api::project_attribute(*attribute, source, &["cfg"]) {
            Some(Some(projected)) => {
                tokens.push("#[".to_owned());
                tokens.extend(projected.key);
                tokens.push("]".to_owned());
            }
            Some(None) => {}
            None => push_tokens(*attribute, source, &mut tokens),
        }
    }
    tokens
}

/// 項目 `item` (`mod` の宣言など) の有る条件 (Issue #305): 項目自身の外側の属性、囲む項目の中身の
/// 内側と外側の属性、ファイルの先頭の内側の属性のうち、`cfg` と `cfg` だけに絞った `cfg_attr` (外側から。
/// 読めない属性があれば None) と、その `cfg`・`cfg_attr` の生のトークン。
pub(crate) fn item_availability(
    item: tree_sitter::Node<'_>,
    source: &[u8],
) -> (Option<Vec<AvailabilityAttr>>, Vec<String>) {
    // inline module の中身の先頭の内側の属性 (`mod m { #![cfg(x)] }`) は、その module 自身に効く。
    let mut attributes: Vec<tree_sitter::Node<'_>> = match item.kind() {
        "mod_item" => item
            .child_by_field_name("body")
            .map(inner_attribute_items)
            .unwrap_or_default()
            .into_iter()
            .rev()
            .collect(),
        _ => Vec::new(),
    };
    attributes.extend(preceding_attribute_items(item));
    attributes.extend(enclosing_attribute_items(item));
    attributes.reverse();
    let mut availability = Some(Vec::new());
    push_availability(&attributes, source, &mut availability);
    (availability, condition_tokens(&attributes, source))
}

/// ファイル (`source_file` の `root`) の先頭の内側の属性のうち、`cfg` と `cfg` に絞った `cfg_attr` (外側
/// から。読めない属性があれば None。Issue #305)。
pub(crate) fn file_availability(
    root: tree_sitter::Node<'_>,
    source: &[u8],
) -> Option<Vec<AvailabilityAttr>> {
    let mut availability = Some(Vec::new());
    push_availability(&inner_attribute_items(root), source, &mut availability);
    availability
}

/// 宣言の項目 `item` を囲む項目とファイルの先頭の、有る条件の属性 (`cfg` と、`cfg` だけに絞った
/// `cfg_attr`)。enum・union の形を、項目の有る設定で比べるのに使う (Issue #291)。読めない属性が
/// あれば None。
pub(super) fn enclosing_conditions(
    item: tree_sitter::Node<'_>,
    source: &[u8],
) -> Option<Vec<super::rust_type_api::Attr>> {
    let mut conditions = Vec::new();
    for attribute in enclosing_attribute_items(item) {
        if let Some(attr) = super::rust_type_api::project_attribute(attribute, source, &["cfg"])? {
            conditions.push(attr);
        }
    }
    Some(conditions)
}

/// 宣言の項目 `item` 自身の本体の内側の属性 (関数の `{ #![cfg(..)] }`・trait の中身の先頭)。
fn own_inner_attribute_items(item: tree_sitter::Node<'_>) -> Vec<tree_sitter::Node<'_>> {
    match item.kind() {
        "function_item" | "trait_item" => item
            .child_by_field_name("body")
            .map(inner_attribute_items)
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// 中身 (`declaration_list`) の内側の属性のうち、`cfg`・`cfg_attr` のトークンの並び。
pub(crate) fn inner_conditions(body: tree_sitter::Node<'_>, source: &[u8]) -> Vec<String> {
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
        inline_public: true,
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
    // 関数・trait 自身の本体の先頭の `#![cfg]` も、宣言の条件 (分岐の判定に使う)。
    let own_inner = match item.kind() {
        "function_item" | "trait_item" => item
            .child_by_field_name("body")
            .map(|body| inner_conditions(body, source))
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    let mut conditions: Vec<Vec<String>> = vec![own_inner, preceding_conditions(item, source)];
    // 有る条件の属性のノードも、内側から集める (宣言自身の本体の内側、宣言の外側、囲む項目の
    // 中身の内側と外側、ファイルの先頭の内側の順)。
    let mut attributes: Vec<tree_sitter::Node<'_>> =
        own_inner_attribute_items(item).into_iter().rev().collect();
    attributes.extend(preceding_attribute_items(item));
    attributes.extend(enclosing_attribute_items(item));
    let mut header_found = false;
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
        if matches!(container.kind(), "impl_item" | "trait_item") && !header_found {
            header_found = true;
            context.container_header = header_tokens(container, source);
        }
        if container.kind() == "impl_item"
            && context.owner.is_empty()
            && let Some(owner) = container.child_by_field_name("type")
        {
            push_tokens(owner, source, &mut context.owner);
        }
        if container.kind() == "mod_item" {
            context.inline_public &=
                crate::engine::symbols::rust_node_has_unrestricted_pub_visibility(
                    container, source,
                );
            if let Some(name) = container
                .child_by_field_name("name")
                .and_then(|name| name.utf8_text(source).ok())
            {
                // `r#api` と `api` は同じ module。
                modules.push(name.strip_prefix("r#").unwrap_or(name).to_owned());
            }
        }
        node = container;
    }
    attributes.reverse();
    let mut availability = Some(Vec::new());
    push_availability(&attributes, source, &mut availability);
    context.availability = availability;
    context.availability_raw = condition_tokens(&attributes, source);
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
    /// 同じキーの宣言が複数あり、変わっていないとみなしたキーの、旧と新の添字の並び。契約が名指す
    /// 名前の束縛は別に比べる (Issue #283)。
    pub(crate) unchanged_groups: Vec<(Vec<usize>, Vec<usize>)>,
    /// `pairs` のうち、条件付きの分岐 (`#[cfg]` の分岐) の代表として選んだ組。ほかの分岐の形は
    /// 比べていない (Issue #291)。
    pub(crate) branch_representatives: HashSet<(usize, usize)>,
    /// 旧新の両方に宣言のあるキーの、旧と新の全ての添字の並び (`#[cfg]` の分岐を代表 1 つに縮めない)。
    /// 項目の有る設定を比べるのに使う (Issue #291)。
    pub(crate) groups: Vec<(Vec<usize>, Vec<usize>)>,
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
            unchanged_groups: Vec::new(),
            branch_representatives: HashSet::new(),
            groups: Vec::new(),
            shared_names: counts
                .into_iter()
                .filter(|(_, (old, new))| *old > 1 || *new > 1)
                .map(|(name, _)| name.to_owned())
                .collect(),
        };
        for (olds, news) in groups.into_values() {
            if !olds.is_empty() && !news.is_empty() {
                pairing.groups.push((olds.clone(), news.clone()));
            }
            match (olds.as_slice(), news.as_slice()) {
                ([], _) => news.iter().for_each(|&j| pairing.added[j] = true),
                (_, []) => olds.iter().for_each(|&i| pairing.removed[i] = true),
                ([i], [j]) => pairing.pairs.push((*i, *j)),
                _ => {
                    // 新しい版で同じ名前の宣言が同じ設定に並ぶ (旧い版では並ばなかった) なら、その
                    // 設定ではビルドできない (Issue #291)。
                    if !exclusive_branches(&news, new_declarations)
                        && exclusive_branches(&olds, old_declarations)
                    {
                        pairing.ambiguous.push((olds, news));
                        continue;
                    }
                    let old_signatures: HashSet<&str> =
                        olds.iter().map(|&i| old[i].2.as_str()).collect();
                    let new_signatures: HashSet<&str> =
                        news.iter().map(|&j| new[j].2.as_str()).collect();
                    // 囲む impl・trait の見出しも分岐の契約 (Issue #291)。
                    let headers = |indices: &[usize], declarations: &[RustDeclarationContext]| {
                        indices
                            .iter()
                            .map(|&k| declarations[k].container_header.clone())
                            .collect::<HashSet<Vec<String>>>()
                    };
                    // どちらの側も条件付きの分岐だけ (1 つなら条件は問わない) なら、分岐をまとめた・
                    // 分けた変更として扱える。signature と囲む impl・trait の見出しがすべて同じなら
                    // 利用者から見た契約は変わらない (どの設定で使えるかの変化は別に比べる)。各側の
                    // signature がそれぞれ 1 種類なら、どの設定でも契約はその 1 つなので、その 2 つを
                    // 比べる。
                    if only_conditional_variants(&olds, old_declarations)
                        && only_conditional_variants(&news, new_declarations)
                        && old_signatures.len() == 1
                        && new_signatures.len() == 1
                        && single_header(
                            &headers(&olds, old_declarations),
                            &headers(&news, new_declarations),
                        )
                    {
                        if old_signatures != new_signatures {
                            pairing.pairs.push((olds[0], news[0]));
                            if olds.len() > 1 || news.len() > 1 {
                                pairing.branch_representatives.insert((olds[0], news[0]));
                            }
                        } else {
                            pairing.unchanged_groups.push((olds, news));
                        }
                        continue;
                    }
                    // 分岐を消しただけか足しただけ (片方の版の (条件, signature, 囲む impl・trait の
                    // 見出し) の組が他方に全て含まれる。足した分岐が並ばないことは上で確かめた) なら、
                    // 残った分岐の契約は同じ。有る設定の変化は別に比べる (Issue #291)。
                    let old_entries = condition_signatures(&olds, old, old_declarations);
                    let new_entries = condition_signatures(&news, new, new_declarations);
                    if includes(&old_entries, &new_entries) || includes(&new_entries, &old_entries)
                    {
                        pairing.unchanged_groups.push((olds, news));
                    } else {
                        pairing.ambiguous.push((olds, news));
                    }
                }
            }
        }
        // 出力の順を新しい版の宣言の順にそろえる。
        pairing.pairs.sort_by_key(|&(_, j)| j);
        pairing
            .groups
            .sort_by_key(|(_, news)| news.first().copied());
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

/// impl・trait の見出しのトークン (中身 (`body`) の前まで。コメントを除く)。
fn header_tokens(container: tree_sitter::Node<'_>, source: &[u8]) -> Vec<String> {
    let body = container.child_by_field_name("body");
    let mut tokens = Vec::new();
    let mut cursor = container.walk();
    for child in container.children(&mut cursor) {
        if Some(child) == body {
            break;
        }
        push_tokens(child, source, &mut tokens);
    }
    tokens
}

/// 宣言 `indices` の有る条件が、どの設定でも 2 つ同時に成り立たないか (同じ名前の宣言が同じ
/// 設定に並ばない)。読めない条件があるか、原子が多すぎれば false。
fn exclusive_branches(indices: &[usize], declarations: &[RustDeclarationContext]) -> bool {
    exclusive_declarations(indices.iter().map(|&index| &declarations[index]))
}

/// 宣言の有る条件が、どの設定でも 2 つ同時に成り立たないか (同じ名前の宣言が同じ設定に並ばない)。
/// 読めない条件があるか、原子が多すぎれば false。
pub(crate) fn exclusive_declarations<'a>(
    declarations: impl IntoIterator<Item = &'a RustDeclarationContext>,
) -> bool {
    use super::rust_cfg_predicate::{Atoms, Predicate};
    let mut atoms = Atoms::default();
    let mut predicates = Vec::new();
    for declaration in declarations {
        let Some(attributes) = &declaration.availability else {
            return false;
        };
        predicates.push(Predicate::All(
            attributes
                .iter()
                .map(|attribute| atoms.presence(&attribute.key))
                .collect(),
        ));
    }
    let Some(mut configs) = atoms.configs() else {
        return false;
    };
    configs.all(|config| {
        predicates
            .iter()
            .filter(|predicate| predicate.holds(config))
            .count()
            <= 1
    })
}

/// 旧新を通じて、囲む impl・trait の見出しが 1 種類だけか (分岐ごとに見出しが違えば、条件と見出しの
/// 対応を保って比べる必要がある)。
fn single_header(old: &HashSet<Vec<String>>, new: &HashSet<Vec<String>>) -> bool {
    old.len() == 1 && old == new
}

/// 並べ替えた多重集合 `larger` が `smaller` を全て含むか。
fn includes<T: Ord>(larger: &[T], smaller: &[T]) -> bool {
    let mut rest = larger.iter().peekable();
    smaller.iter().all(|wanted| {
        while rest.next_if(|candidate| *candidate < wanted).is_some() {}
        rest.next_if(|candidate| *candidate == wanted).is_some()
    })
}

/// 同じキーの宣言 `indices` の、cfg の条件と signature と囲む impl・trait の見出しの組 (並べ替え済み)。
fn condition_signatures<'a>(
    indices: &[usize],
    symbols: &'a [(String, String, String)],
    declarations: &'a [RustDeclarationContext],
) -> Vec<(&'a [String], &'a str, &'a [String])> {
    let mut entries: Vec<(&[String], &str, &[String])> = indices
        .iter()
        .map(|&k| {
            (
                declarations[k].conditions.as_slice(),
                symbols[k].2.as_str(),
                declarations[k].container_header.as_slice(),
            )
        })
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
    /// 契約が名指す名前の指す先が変わったか、同じと確かめられなかったときの理由 (Issue #283)。
    /// あれば、互換・変更なし・追随済みとせず blocking にする。
    pub(crate) dependency: Option<crate::models::review::ApiModificationUnverifiedReason>,
    /// 旧新の宣言がそれぞれ 1 つ (`#[cfg]` の分岐の代表でない) の同じファイルの組で、項目の有る
    /// 設定の変化を別に比べる (Issue #291)。このとき enum・union の形は、新しい版に無い設定では
    /// 比べない (消える設定は削除と同じ区分で出す)。分岐の代表の組では、ほかの分岐の形を比べ
    /// られないので false。
    pub(crate) availability_compared: bool,
    /// `#[cfg]` の分岐の代表の組で、構造体・enum・union の形を分岐の組ごとに比べた結果
    /// ([`branch_shape_change`])。あれば代表の組の形の比較に代える (Issue #293)。
    pub(crate) shape_override: Option<super::rust_type_api::ShapeChange>,
}

/// 旧新の宣言が、ある設定で同時に有り得るか (読めない条件か、原子が多すぎれば有り得るとする)。
fn coexist(old: &RustDeclarationContext, new: &RustDeclarationContext) -> bool {
    use super::rust_cfg_predicate::{Atoms, Predicate};
    let (Some(old_attributes), Some(new_attributes)) = (&old.availability, &new.availability)
    else {
        return true;
    };
    let mut atoms = Atoms::default();
    let mut predicate = |attributes: &[AvailabilityAttr]| {
        Predicate::All(
            attributes
                .iter()
                .map(|attribute| atoms.presence(&attribute.key))
                .collect(),
        )
    };
    let (old_predicate, new_predicate) = (predicate(old_attributes), predicate(new_attributes));
    let Some(mut configs) = atoms.configs() else {
        return true;
    };
    configs.any(|config| old_predicate.holds(config) && new_predicate.holds(config))
}

/// `#[cfg]` の分岐に分けた構造体・enum・union (`kind`) の形を、有る条件の重なる旧新の分岐の組ごとに、
/// 両方が有る設定で比べ、最も重い変化を返す (Issue #293)。代表の 1 組では、ほかの分岐の設定の形
/// (signature の正規化で失われるフィールドの順など) を比べられないため。旧い分岐にしか無い設定は、
/// 有る設定の比較 (Issue #291) で出す。形を読めない宣言があれば None。
pub(crate) fn branch_shape_change(
    kind: &str,
    old_source: &[u8],
    new_source: &[u8],
    old: &[&RustDeclarationContext],
    new: &[&RustDeclarationContext],
) -> Option<super::rust_type_api::ShapeChange> {
    let mut change = super::rust_type_api::ShapeChange::Unchanged;
    for old_declaration in old {
        for new_declaration in new {
            if !coexist(old_declaration, new_declaration) {
                continue;
            }
            let (old_start, new_start) = (old_declaration.start, new_declaration.start);
            let pair = match kind {
                "struct" => super::rust_struct_api::classify(
                    &struct_shape_at(old_source, old_start)?,
                    &struct_shape_at(new_source, new_start)?,
                    true,
                ),
                "enum" => super::rust_enum_api::classify(
                    &enum_shape_at(old_source, old_start)?,
                    &enum_shape_at(new_source, new_start)?,
                    true,
                ),
                "union" => super::rust_union_api::classify(
                    &union_shape_at(old_source, old_start)?,
                    &union_shape_at(new_source, new_start)?,
                    true,
                ),
                _ => return None,
            };
            change = change.max(pair);
        }
    }
    Some(change)
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
    let item = item_at(tree.root_node(), start, "struct_item")?;
    super::rust_struct_api::RustStructShape::from_node(item, source)?
        .with_enclosing(enclosing_conditions(item, source)?)
        .into()
}

/// `source` の `start` に始まる enum の形。
pub(crate) fn enum_shape_at(
    source: &[u8],
    start: (usize, usize),
) -> Option<super::rust_enum_api::RustEnumShape> {
    let tree = parser::parse_source(source, crate::language::LangId::Rust).ok()?;
    let item = item_at(tree.root_node(), start, "enum_item")?;
    super::rust_enum_api::RustEnumShape::from_node(item, source)?
        .with_enclosing(enclosing_conditions(item, source)?)
        .into()
}

/// `source` の `start` に始まる union の形。
pub(crate) fn union_shape_at(
    source: &[u8],
    start: (usize, usize),
) -> Option<super::rust_union_api::RustUnionShape> {
    let tree = parser::parse_source(source, crate::language::LangId::Rust).ok()?;
    let item = item_at(tree.root_node(), start, "union_item")?;
    super::rust_union_api::RustUnionShape::from_node(item, source)?
        .with_enclosing(enclosing_conditions(item, source)?)
        .into()
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
            super::super::rust_struct_api::classify(&first, &second, false),
            super::super::rust_type_api::ShapeChange::Unchanged
        );
        assert!(struct_shape_at(source, (0, 0)).is_none());
    }
}
