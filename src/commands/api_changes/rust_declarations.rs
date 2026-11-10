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
        // 囲む impl の対象の型だけが変わった項目 (`impl<T> S<T>` → `impl S<u8>`・型の引数の改名) は、
        // 対象の型の字句がキーに入るので組にならない。残った削除と追加のうち、(inline module, 対象の型の
        // 名前 (型の引数を除く), qualname, 種類) が同じで 1 つずつのものを組にし、見出しを比べる (Issue #307)。
        let mut unpaired: std::collections::BTreeMap<Key<'_>, (Vec<usize>, Vec<usize>)> =
            std::collections::BTreeMap::new();
        for (i, ((name, kind, _), declaration)) in old.iter().zip(old_declarations).enumerate() {
            if pairing.removed[i] && !declaration.owner.is_empty() {
                let key = (
                    declaration.inline_module.as_str(),
                    owner_base(&declaration.owner),
                    name.as_str(),
                    kind.as_str(),
                );
                unpaired.entry(key).or_default().0.push(i);
            }
        }
        for (j, ((name, kind, _), declaration)) in new.iter().zip(new_declarations).enumerate() {
            if pairing.added[j] && !declaration.owner.is_empty() {
                let key = (
                    declaration.inline_module.as_str(),
                    owner_base(&declaration.owner),
                    name.as_str(),
                    kind.as_str(),
                );
                unpaired.entry(key).or_default().1.push(j);
            }
        }
        for (olds, news) in unpaired.into_values() {
            match (olds.as_slice(), news.as_slice()) {
                ([], _) | (_, []) => {}
                ([i], [j]) => {
                    pairing.removed[*i] = false;
                    pairing.added[*j] = false;
                    pairing.pairs.push((*i, *j));
                    pairing.groups.push((vec![*i], vec![*j]));
                }
                // 1 つの impl を複数の対象の型の impl に分けたか、まとめた (`impl<T> S<T>` →
                // `impl S<u8>` と `impl S<u16>`)。どの宣言どうしを比べるか決められないので blocking。
                _ => {
                    olds.iter().for_each(|&i| pairing.removed[i] = false);
                    news.iter().for_each(|&j| pairing.added[j] = false);
                    pairing.ambiguous.push((olds, news));
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

/// impl の対象の型のトークン `owner` の、型の引数の前まで (`S<T>` の `S`)。先頭の `self::` は除く (同じ
/// module の同じ名前)。ほかの経路は残す: 同じファイルの中で `a::S` を `b::S` に付け替えた impl は別の型の
/// impl なので組にしない (経路の書き方だけを変えた impl も、同じ型と確かめられないので組にしない)。
fn owner_base(owner: &[String]) -> &[String] {
    let end = owner
        .iter()
        .position(|token| token == "<")
        .unwrap_or(owner.len());
    let start = if end > 2 && owner[0] == "self" && owner[1] == "::" {
        2
    } else {
        0
    };
    &owner[start..end]
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
    /// 囲む inherent impl の見出し (対象の型・型の引数・`where`) を旧新で比べた結果 ([`ImplHeaders`])。
    /// 見出しの字句が変わったときだけ求める (Issue #307)。
    pub(crate) impl_header: Option<super::rust_type_api::ShapeChange>,
    /// 囲む inherent impl の見出しの型の引数の名前を付け直すと、signature の契約の部分が同じ (signature の
    /// 字面の違いは付け直しと整形だけ。Issue #307)。
    pub(crate) impl_member_renamed: bool,
    /// 関連定数の値のマクロの引数の中の型の引数の対応が旧新で変わり、値が同じと言えない (Issue #307)。
    pub(crate) impl_value_changed: bool,
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

/// 構文木を作ったソース (囲む impl の見出しを比べるのに、ファイルごとに 1 度だけ作って共有する)。
pub(crate) struct ParsedSource {
    tree: tree_sitter::Tree,
    source: Vec<u8>,
    /// impl の始まりのバイト → 見出しと名前の付け直し (メソッドごとに作り直さない)。
    impls: std::cell::RefCell<std::collections::HashMap<usize, ImplEntry>>,
}

/// inherent impl の見出しと名前の付け直し (読めなければ None)。
type ImplEntry = Option<std::rc::Rc<(ImplHeader, ImplNames)>>;

impl ParsedSource {
    pub(crate) fn parse(source: Vec<u8>) -> Option<std::rc::Rc<Self>> {
        let tree = parser::parse_source(&source, crate::language::LangId::Rust).ok()?;
        Some(std::rc::Rc::new(Self {
            tree,
            source,
            impls: std::cell::RefCell::default(),
        }))
    }

    /// `start` に始まる項目を囲む inherent impl の対象の型を、型の名前の前の経路を除き、impl の型の
    /// 引数を位置の名前 ([`ImplNames`]) に置き換えた字句 (`impl<V> crate::S<V, u16>` の `S<#0, u16>`)。
    /// 移動の組を、型の引数の名前と経路の書き方に依らずに作る。inherent impl の中に無いか、見出しを
    /// 読めなければ None。
    pub(crate) fn normalized_owner(&self, start: (usize, usize)) -> Option<Vec<String>> {
        let item = inherent_impl_at(self.tree.root_node(), start)?;
        let found = self.impl_names(item)?;
        let (header, names) = &*found;
        Some(last_segment(applied(&names.self_type, &header.self_type)))
    }

    /// inherent impl `item` の見出しと名前の付け直し (impl ごとに 1 度だけ作る)。読めなければ None。
    fn impl_names(&self, item: tree_sitter::Node<'_>) -> ImplEntry {
        let key = item.start_byte();
        if let Some(found) = self.impls.borrow().get(&key) {
            return found.clone();
        }
        let found = impl_header(item, &self.source).map(|header| {
            let names = ImplNames::new(&header);
            std::rc::Rc::new((header, names))
        });
        self.impls.borrow_mut().insert(key, found.clone());
        found
    }
}

/// 旧新のソースの、項目を囲む inherent impl の見出しを比べる (Issue #307)。
pub(crate) struct ImplHeaders {
    old: std::rc::Rc<ParsedSource>,
    new: std::rc::Rc<ParsedSource>,
    /// (旧新の impl の始まりのバイト, 旧新の対象の型・メンバーで番号の決まる引数の並び) → 見出しの変化の
    /// 最も小さいもの (並べ替えの探索を 1 度だけにする)。
    searched: std::cell::RefCell<std::collections::HashMap<SearchKey, Searched>>,
}

/// [`ImplHeaders::search`] の結果: 見出しの変化の最も小さいものと、それを与える新の並び (番号付けの並びの
/// ままなら None)。
type Searched = (super::rust_type_api::ShapeChange, Option<Vec<usize>>);

/// [`ImplHeaders::search`] の結果を覚える鍵。
type SearchKey = ((usize, usize), Vec<usize>, Vec<usize>);

/// inherent impl の見出し: 対象の型の字句と、型の引数・`where`。
struct ImplHeader {
    self_type: super::rust_type_api::Tokens,
    generics: super::rust_type_api::Generics,
}

/// impl の型の引数の名前の付け直し。引数は宣言の順ではなく対象の型に現れる順で番号を付け (`impl<U, T>
/// S<T, U>` の並べ替えを変更にしない)、対象の型に現れない引数は境界と述語の中の位置で付ける。型の位置
/// (`S<N>`・`Bound<N>` の `N`) に書いた const の引数は、同じ名前の型と区別できない (マクロや derive が作る型も
/// ある) ので置き換えない (名前で比べ、改名すれば違いになる)。値の位置 (`{ N }`・`[u8; N]`) では置き換える。
struct ImplNames {
    /// 対象の型を比べる置き換え。
    self_type: super::rust_type_api::Renaming,
    /// 境界と述語を比べる置き換え。
    bounds: super::rust_type_api::Renaming,
    /// 引数の添字を、付け直した番号の順に並べたもの。
    order: Vec<usize>,
    /// `order` の先頭の、対象の型に現れる (対象の型で番号の決まる) 引数の数。
    fixed: usize,
    /// `order` の先頭の、使われる引数の数 (後ろは使われない寿命の引数で、比べない)。
    used: usize,
}

/// 書き順に依らない番号付けをする、対象の型に現れない引数の数の上限 (超えれば宣言の順)。
const MAX_REFINED_PARAMS: usize = 16;

/// 旧に対する新の並びを入れ替えて比べる、対象の型に現れない引数の数の上限 (6! = 720 通り)。
const MAX_PERMUTED_PARAMS: usize = 6;

impl ImplNames {
    fn new(header: &ImplHeader) -> Self {
        Self::anchored(header, &[], &[])
    }

    /// 対象の型と `anchors` (メンバーの signature の字句) に現れる順に、次に `by_name` (`r#` と寿命の `'` を外した
    /// 名前) の順に番号を付け、残りは境界と述語の中の形で付ける。メンバーの signature に現れる引数は、そこでの
    /// 位置が呼び出し側から見た引数の同一性になる。マクロの引数の中に現れる引数は名前を付け直せないので、名前で
    /// 組にする (`by_name` に渡す)。
    fn anchored(
        header: &ImplHeader,
        anchors: &[&super::rust_type_api::Tokens],
        by_name: &[OpaqueName],
    ) -> Self {
        use super::rust_type_api::{ParamKind, Role};
        let params = &header.generics.params;
        let index_of: std::collections::HashMap<&str, usize> = params
            .iter()
            .enumerate()
            .map(|(index, param)| (param.name.as_str(), index))
            .collect();
        // 番号を付けた引数の並び (`order`) と、引数ごとの番号 (`rank`)。
        let mut order: Vec<usize> = Vec::new();
        let mut rank: Vec<Option<usize>> = vec![None; params.len()];
        // 1. 対象の型と `anchors` に現れる順。型の引数を名指し得る役割の字句だけを見る (`T::A` の関連型の
        //    名前 `A` は、同じ名前の型の引数ではない)。
        for tokens in std::iter::once(&header.self_type).chain(anchors.iter().copied()) {
            for token in &tokens.0 {
                if matches!(
                    token.role,
                    Role::TypeName | Role::ValueName | Role::Lifetime
                ) && let Some(&index) = index_of.get(token.compared())
                    && rank[index].is_none()
                {
                    rank[index] = Some(order.len());
                    order.push(index);
                }
            }
        }
        for name in by_name {
            if let Some(index) = params.iter().position(|param| is_named(param, name))
                && rank[index].is_none()
            {
                rank[index] = Some(order.len());
                order.push(index);
            }
        }
        let fixed = order.len();
        // 使われない寿命の引数 (対象の型・`anchors`・`by_name`・ほかの引数の境界・述語のどれにも現れない) は、
        // どの寿命でも満たせるので比べない (番号を付けない)。`impl<'a> S` を `impl S` にしても変えない。
        let appears_in = |tokens: &super::rust_type_api::Tokens, name: &str| {
            tokens.0.iter().any(|token| token.compared() == name)
        };
        let unused: Vec<bool> = params
            .iter()
            .enumerate()
            .map(|(index, param)| {
                param.kind == ParamKind::Lifetime
                    && rank[index].is_none()
                    && !params.iter().enumerate().any(|(other, other_param)| {
                        other != index
                            && other_param
                                .bounds
                                .iter()
                                .any(|bound| appears_in(bound, &param.name))
                    })
                    && !header.generics.predicates.iter().any(|(left, bound)| {
                        appears_in(left, &param.name) || appears_in(bound, &param.name)
                    })
            })
            .collect();
        // 2. 対象の型に現れない引数 (関連型の等式 `T: Tr<A = U>` で決まる、など) は、境界と述語の中の形で
        //    番号を付ける。番号の無い引数ごとに、それが現れる境界・述語を、その引数を `*`、番号の付いた引数を
        //    番号、ほかの番号の無い引数を `?` に置き換えた字面の集まり (整列) を作り、番号の付いた引数と同じ
        //    境界に現れるものを先に、字面の集まりの小さい順に 1 つずつ番号を付ける。境界・述語の書き順に
        //    依らない (字面の集まりまで同じ引数どうしだけ、宣言の順)。
        let constraints: Vec<Vec<(&str, Option<usize>)>> = params
            .iter()
            .flat_map(|param| {
                param.bounds.iter().map(move |bound| {
                    std::iter::once(param.name.as_str())
                        .chain(std::iter::once(":"))
                        .chain(bound.0.iter().map(|token| token.compared()))
                        .collect::<Vec<&str>>()
                })
            })
            .chain(header.generics.predicates.iter().map(|(left, bound)| {
                left.0
                    .iter()
                    .map(|token| token.compared())
                    .chain(std::iter::once(":"))
                    .chain(bound.0.iter().map(|token| token.compared()))
                    .collect::<Vec<&str>>()
            }))
            .map(|texts| {
                texts
                    .into_iter()
                    .map(|text| (text, index_of.get(text).copied()))
                    .collect()
            })
            .collect();
        let mut appears: Vec<Vec<usize>> = vec![Vec::new(); params.len()];
        for (at, constraint) in constraints.iter().enumerate() {
            for &(_, index) in constraint {
                if let Some(index) = index
                    && appears[index].last() != Some(&at)
                {
                    appears[index].push(at);
                }
            }
        }
        // 番号の無い引数を並べる鍵: 番号の付いた引数と一緒に現れる境界が無いか、その境界の整列した字面の集まり、
        // 番号の無いほかの引数と一緒に現れる境界の集まり、その引数だけの境界の集まり。後ろの 2 つ
        // (`V: AsRef<W>`・`U: Copy`) は足し外しされやすいので、同点を分けるのにだけ使う。
        type Shape = (bool, Vec<Vec<String>>, Vec<Vec<String>>, Vec<Vec<String>>);
        // 番号の無い引数が多ければ (書き順に依らない番号付けの手間が大きい)、宣言の順にする。
        let refine = params.len() - fixed <= MAX_REFINED_PARAMS;
        loop {
            if !refine {
                break;
            }
            let mut best: Option<(Shape, usize)> = None;
            for candidate in 0..params.len() {
                if rank[candidate].is_some() || appears[candidate].is_empty() || unused[candidate] {
                    continue;
                }
                let mut numbered: Vec<Vec<String>> = Vec::new();
                let mut related: Vec<Vec<String>> = Vec::new();
                let mut own: Vec<Vec<String>> = Vec::new();
                for &at in &appears[candidate] {
                    let (mut others, mut anchor) = (false, false);
                    let rendered: Vec<String> = constraints[at]
                        .iter()
                        .map(|&(text, index)| match index {
                            Some(index) if index == candidate => "*".to_owned(),
                            Some(index) => {
                                others = true;
                                match rank[index] {
                                    Some(number) => {
                                        anchor = true;
                                        format!("@{number}")
                                    }
                                    None => "?".to_owned(),
                                }
                            }
                            None => text.to_owned(),
                        })
                        .collect();
                    if anchor {
                        numbered.push(rendered);
                    } else if others {
                        related.push(rendered);
                    } else {
                        own.push(rendered);
                    }
                }
                numbered.sort();
                related.sort();
                own.sort();
                let key = (numbered.is_empty(), numbered, related, own);
                if best.as_ref().is_none_or(|(best, _)| key < *best) {
                    best = Some((key, candidate));
                }
            }
            let Some((_, candidate)) = best else {
                break;
            };
            rank[candidate] = Some(order.len());
            order.push(candidate);
        }
        // 3. どこにも現れない引数は宣言の順。使われない寿命の引数は最後に (相手に無くてもよい)。
        for (index, &unused) in unused.iter().enumerate() {
            if !unused && !order.contains(&index) {
                order.push(index);
            }
        }
        let used = order.len();
        order.extend(
            unused
                .iter()
                .enumerate()
                .filter(|(_, unused)| **unused)
                .map(|(index, _)| index),
        );
        Self::with_order(header, order, fixed, used)
    }

    /// 引数に `order` の順に番号を付けた付け直し (`fixed` は先頭の、対象の型で番号の決まる引数の数)。
    fn with_order(header: &ImplHeader, order: Vec<usize>, fixed: usize, used: usize) -> Self {
        use super::rust_type_api::{ParamKind, Renaming};
        let params = &header.generics.params;
        let mut bounds = Renaming {
            types: std::collections::HashMap::new(),
            values: std::collections::HashMap::new(),
            lifetimes: std::collections::HashMap::new(),
        };
        let (mut lifetimes, mut others) = (0usize, 0usize);
        for &index in &order {
            let param = &params[index];
            match param.kind {
                ParamKind::Lifetime => {
                    bounds
                        .lifetimes
                        .insert(param.name.clone(), format!("'#{lifetimes}"));
                    lifetimes += 1;
                }
                ParamKind::Type => {
                    bounds
                        .types
                        .insert(param.name.clone(), format!("#{others}"));
                    others += 1;
                }
                _ => {
                    bounds
                        .values
                        .insert(param.name.clone(), format!("#{others}"));
                    others += 1;
                }
            }
        }
        let self_type = Renaming {
            types: bounds.types.clone(),
            values: bounds.values.clone(),
            lifetimes: bounds.lifetimes.clone(),
        };
        Self {
            self_type,
            bounds,
            order,
            fixed,
            used,
        }
    }
}

impl ImplHeaders {
    pub(crate) fn new(old: Vec<u8>, new: Vec<u8>) -> Option<Self> {
        Some(Self::of(
            ParsedSource::parse(old)?,
            ParsedSource::parse(new)?,
        ))
    }

    /// 作った構文木を共有して比べる。
    pub(crate) fn of(old: std::rc::Rc<ParsedSource>, new: std::rc::Rc<ParsedSource>) -> Self {
        Self {
            old,
            new,
            searched: std::cell::RefCell::default(),
        }
    }

    /// 旧新の `start` に始まる項目を囲む inherent impl (trait の impl は除く) の見出しを比べた結果と、
    /// 旧新の見出しの字面 (表示用)。対象の型が (型の引数の付け直しと、型の名前の前の経路を除いて) 違えば
    /// 壊す。型の引数と `where` は impl の規則で比べる ([`compare_impl_generics`])。どちらも inherent impl の
    /// 中でなければ None。片方だけか、読めなければ壊す。
    pub(crate) fn change(
        &self,
        old_start: (usize, usize),
        new_start: (usize, usize),
    ) -> Option<ImplHeaderChange> {
        use super::rust_type_api::ShapeChange;
        let old_impl = inherent_impl_at(self.old.tree.root_node(), old_start);
        let new_impl = inherent_impl_at(self.new.tree.root_node(), new_start);
        let (old_impl, new_impl) = match (old_impl, new_impl) {
            (None, None) => return None,
            (Some(old_impl), Some(new_impl)) => (old_impl, new_impl),
            (old_impl, new_impl) => {
                let text = |node: Option<tree_sitter::Node<'_>>, source: &[u8]| {
                    node.map(|node| impl_header_text(node, source))
                        .unwrap_or_default()
                };
                return Some(ImplHeaderChange {
                    old_text: text(old_impl, &self.old.source),
                    new_text: text(new_impl, &self.new.source),
                    ..ImplHeaderChange::breaking()
                });
            }
        };
        let mut result = ImplHeaderChange {
            old_text: impl_header_text(old_impl, &self.old.source),
            new_text: impl_header_text(new_impl, &self.new.source),
            ..ImplHeaderChange::breaking()
        };
        let (Some(old), Some(new)) = (self.old.impl_names(old_impl), self.new.impl_names(new_impl))
        else {
            return Some(result);
        };
        let ((old_header, old_names), (new_header, new_names)) = (&*old, &*new);
        let self_type = |header: &ImplHeader, names: &ImplNames| {
            last_segment(applied(&names.self_type, &header.self_type))
        };
        if self_type(old_header, old_names) != self_type(new_header, new_names) {
            return Some(result);
        }
        // メンバーの signature の字句 (付け直す前)。
        let old_member = member_at(self.old.tree.root_node(), old_start)
            .and_then(|member| member_signature(member, &self.old.source));
        let new_member = member_at(self.new.tree.root_node(), new_start)
            .and_then(|member| member_signature(member, &self.new.source));
        // マクロの引数の中 (字面のまま比べる) に現れる型の引数 (名前空間と、`r#` と寿命の `'` を外した名前。旧新
        // どちらかに現れたもの) は名前を付け直せないので、名前で組にする。見出しとメンバーの契約の部分と、定数の
        // 値に分ける。
        let opaque = |old: Vec<&super::rust_type_api::Tokens>,
                      new: Vec<&super::rust_type_api::Tokens>| {
            opaque_params(old_header, old)
                .into_iter()
                .chain(opaque_params(new_header, new))
                .collect::<std::collections::BTreeSet<OpaqueName>>()
        };
        let contract_opaque = opaque(
            contract_tokens(old_header, &old_member),
            contract_tokens(new_header, &new_member),
        );
        let value_opaque = opaque(value_tokens(&old_member), value_tokens(&new_member));
        let by_name: Vec<OpaqueName> = contract_opaque.union(&value_opaque).cloned().collect();
        // メンバーの signature に対象の型に現れない引数が現れるか、マクロの引数の中に型の引数が現れれば、そこでの
        // 位置と名前で番号を付け直す (呼び出し側から見た引数の同一性)。どちらも無ければ impl ごとの付け直しのまま。
        let member_names =
            |member: &Option<MemberSignature>, header: &ImplHeader, base: &ImplNames| {
                use super::rust_type_api::Role;
                let pieces: Vec<&super::rust_type_api::Tokens> = member
                    .as_ref()
                    .map(|(pieces, ..)| pieces.iter().collect())
                    .unwrap_or_default();
                let fixed: std::collections::HashSet<usize> =
                    base.order[..base.fixed].iter().copied().collect();
                let free: std::collections::HashSet<&str> = header
                    .generics
                    .params
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| !fixed.contains(index))
                    .map(|(_, param)| param.name.as_str())
                    .collect();
                let mentions_free = pieces.iter().any(|piece| {
                    piece.0.iter().any(|token| {
                        matches!(
                            token.role,
                            Role::TypeName | Role::ValueName | Role::Lifetime
                        ) && free.contains(token.compared())
                    })
                });
                (mentions_free || !by_name.is_empty())
                    .then(|| ImplNames::anchored(header, &pieces, &by_name))
            };
        let old_anchored = member_names(&old_member, old_header, old_names);
        let new_anchored = member_names(&new_member, new_header, new_names);
        let old_names = old_anchored.as_ref().unwrap_or(old_names);
        let new_names = new_anchored.as_ref().unwrap_or(new_names);
        let renamed = |member: &Option<MemberSignature>, names: &ImplNames| {
            member.as_ref().map(|(pieces, value, _)| {
                (
                    pieces
                        .iter()
                        .map(|piece| applied(&names.bounds, piece))
                        .collect::<Vec<_>>(),
                    value.as_ref().map(|value| applied(&names.bounds, value)),
                )
            })
        };
        // 見出しの変化の最も小さい対応 (並べ替えの探索で採った新の並び) で、以下のマクロ・メンバー・値を比べる。
        let (header_change, adopted) = self.search(
            (old_impl.start_byte(), new_impl.start_byte()),
            (old_header, old_names),
            (new_header, new_names),
        );
        let adopted = adopted
            .map(|order| ImplNames::with_order(new_header, order, new_names.fixed, new_names.used));
        let new_names = adopted.as_ref().unwrap_or(new_names);
        // 名前で組にした引数の番号が旧新で違えば (同じ名前が対象の型やメンバーの別の位置の引数を指す、片方にしか
        // 無い)、対応を確かめられない。
        let conflict = |names: &std::collections::BTreeSet<OpaqueName>| {
            names.iter().any(|name| {
                plain_number(old_names, old_header, name)
                    != plain_number(new_names, new_header, name)
            })
        };
        // マクロの呼び出しは、引数に書かない型の引数も呼び出し側の名前で使える (`macro_rules! ty { () => { T } }`)。
        // 名前の付け直しが旧新で違えば (改名・並べ替え)、マクロの呼び出しを含む契約の部分は対応を確かめられない
        // ので壊し、値は同じとしない (値の変更として別に比べる)。
        let has_macro = |tokens: Vec<&super::rust_type_api::Tokens>| {
            tokens.iter().any(|tokens| {
                tokens
                    .0
                    .iter()
                    .any(|token| token.role == super::rust_type_api::Role::Opaque)
            })
        };
        let renaming_differs = !same_renaming(&old_names.bounds, &new_names.bounds);
        let mut member_breaking = conflict(&contract_opaque)
            || renaming_differs
                && (has_macro(contract_tokens(old_header, &old_member))
                    || has_macro(contract_tokens(new_header, &new_member)));
        // 型の位置に書いた const の引数は名前のまま比べる (同じ名前の型と区別できない) ので、その名前の番号が旧新で
        // 違えば (位置の入れ替え・片方にしか無い)、同じ字面が別の引数を指し、対応を確かめられない。見出しとメンバーの
        // 契約の部分なら壊し、定数の値の中だけなら値が同じとはしない (値の変更として別に比べる)。
        let typed_consts = |header: &ImplHeader, tokens: Vec<&super::rust_type_api::Tokens>| {
            use super::rust_type_api::{ParamKind, Role};
            let consts: std::collections::HashSet<&str> = header
                .generics
                .params
                .iter()
                .filter(|param| param.kind == ParamKind::Const)
                .map(|param| param.name.as_str())
                .collect();
            tokens
                .into_iter()
                .flat_map(|tokens| tokens.0.iter())
                .filter(|token| token.role == Role::TypeName && consts.contains(token.compared()))
                .map(|token| token.compared().to_owned())
                .collect::<std::collections::BTreeSet<String>>()
        };
        let renumbered = |old: std::collections::BTreeSet<String>,
                          new: std::collections::BTreeSet<String>| {
            old.union(&new)
                .any(|name| old_names.bounds.values.get(name) != new_names.bounds.values.get(name))
        };
        if renumbered(
            typed_consts(old_header, contract_tokens(old_header, &old_member)),
            typed_consts(new_header, contract_tokens(new_header, &new_member)),
        ) {
            member_breaking = true;
        }
        let value_renumbered = renumbered(
            typed_consts(old_header, value_tokens(&old_member)),
            typed_consts(new_header, value_tokens(&new_member)),
        );
        // メンバーの signature を付け直して比べる。契約の部分 (関数の本体と引数のパターンを除く部分、定数の型) が
        // 同じなら、signature の字面の違いは付け直しと整形 (省略できるカンマなど) だけ (定数は値も同じとき)。
        // 名前の付け直しが旧新で違えば字面は比べられないので、契約の部分が違うか読めなければ壊し、値が違えば
        // signature の字面が同じでも値の変更にする。付け直しが同じなら、違いの判定は signature の比較に任せる。
        if !member_breaking {
            match (
                renamed(&old_member, old_names),
                renamed(&new_member, new_names),
            ) {
                (Some((old_contract, old_value)), Some((new_contract, new_value)))
                    if old_contract == new_contract =>
                {
                    let value_same = old_value == new_value
                        && !conflict(&value_opaque)
                        && !value_renumbered
                        && !(renaming_differs
                            && (has_macro(value_tokens(&old_member))
                                || has_macro(value_tokens(&new_member))));
                    result.member_renamed = value_same;
                    result.value_changed = renaming_differs && !value_same;
                }
                _ => member_breaking = renaming_differs,
            }
        }
        // 戻り値が隠れた型 (`impl Trait`・`async`) の関数は、付け直しが同じでも引数と本体を (付け直して) 比べる。
        // 違えば (局所の名前の改名だけでも) 隠れた型が同じと確かめられないので壊す。
        let hidden = |member: &Option<MemberSignature>| {
            member.as_ref().is_some_and(|(_, _, hidden)| *hidden)
        };
        if !member_breaking
            && !renaming_differs
            && (hidden(&old_member) || hidden(&new_member))
            && renamed(&old_member, old_names).map(|(contract, _)| contract)
                != renamed(&new_member, new_names).map(|(contract, _)| contract)
        {
            member_breaking = true;
        }
        // 隠れた型の関数は、見出しの境界を変えると本体の名前やメソッドの解決が変わり得る (境界の trait のメソッドが
        // 先に選ばれる) ので、見出しが変われば (緩めても) 壊す。
        if (hidden(&old_member) || hidden(&new_member)) && header_change != ShapeChange::Unchanged {
            member_breaking = true;
        }
        result.change = if member_breaking {
            ShapeChange::Breaking
        } else {
            header_change
        };
        Some(result)
    }

    /// 旧の見出しと付け直し `old` に対する、新の見出し `new` の変化の最も小さいもの (impl の組 `impls` と、
    /// 対象の型・メンバーで番号の決まる引数の並びごとに 1 度だけ求める)。それ以外の引数 (関連型の等式など
    /// で決まる) には書き方に依らない番号が無いので、6 個までなら新の並びを入れ替えて探す (境界を削っただけで
    /// 番号が入れ替わり、強めたと読まない)。どう並べても壊すと分かれば ([`surely_breaking`]) 探さない。
    fn search(
        &self,
        impls: (usize, usize),
        (old_header, old_names): (&ImplHeader, &ImplNames),
        (new_header, new_names): (&ImplHeader, &ImplNames),
    ) -> Searched {
        use super::rust_type_api::ShapeChange;
        let key = (
            impls,
            old_names.order[..old_names.fixed].to_vec(),
            new_names.order[..new_names.fixed].to_vec(),
        );
        if let Some(found) = self.searched.borrow().get(&key) {
            return found.clone();
        }
        let header_change = |names: &ImplNames| {
            compare_impl_generics(&old_header.generics, &new_header.generics, old_names, names)
        };
        let mut best = header_change(new_names);
        let mut adopted = None;
        let fixed = new_names.fixed;
        let free = new_names.used - fixed;
        if best != ShapeChange::Unchanged
            && old_names.fixed == fixed
            && old_names.used == new_names.used
            && (2..=MAX_PERMUTED_PARAMS).contains(&free)
            && !surely_breaking((old_header, old_names), (new_header, new_names))
        {
            let used = new_names.used;
            let mut tail = new_names.order[fixed..used].to_vec();
            for_each_permutation(&mut tail, |permuted| {
                if permuted == &new_names.order[fixed..used] {
                    return false;
                }
                let order: Vec<usize> = new_names.order[..fixed]
                    .iter()
                    .chain(permuted)
                    .chain(&new_names.order[used..])
                    .copied()
                    .collect();
                let names = ImplNames::with_order(new_header, order.clone(), fixed, used);
                let change = header_change(&names);
                if change < best {
                    best = change;
                    adopted = Some(order);
                }
                best == ShapeChange::Unchanged
            });
        }
        self.searched
            .borrow_mut()
            .insert(key, (best, adopted.clone()));
        (best, adopted)
    }
}

/// 対象の型で番号の決まる引数の境界と述語のうち、対象の型に現れない引数を名指さないものを、新が旧に無いもの
/// として足していれば真 (対象の型に現れない引数をどう並べても壊す)。
fn surely_breaking(
    (old_header, old_names): (&ImplHeader, &ImplNames),
    (new_header, new_names): (&ImplHeader, &ImplNames),
) -> bool {
    use std::collections::{BTreeSet, HashSet};
    let free_names = |header: &ImplHeader, names: &ImplNames| -> HashSet<String> {
        names.order[names.fixed..names.used]
            .iter()
            .map(|&index| header.generics.params[index].name.clone())
            .collect()
    };
    let (old_free, new_free) = (
        free_names(old_header, old_names),
        free_names(new_header, new_names),
    );
    let mentions = |tokens: &super::rust_type_api::Tokens, free: &HashSet<String>| {
        tokens.0.iter().any(|token| free.contains(token.compared()))
    };
    for at in 0..new_names.fixed.min(old_names.fixed) {
        let old_param = &old_header.generics.params[old_names.order[at]];
        let new_param = &new_header.generics.params[new_names.order[at]];
        let old_bounds: BTreeSet<Vec<String>> = old_param
            .bounds
            .iter()
            .filter(|bound| !mentions(bound, &old_free))
            .map(|bound| applied(&old_names.bounds, bound))
            .collect();
        if new_param
            .bounds
            .iter()
            .filter(|bound| !mentions(bound, &new_free))
            .any(|bound| !old_bounds.contains(&applied(&new_names.bounds, bound)))
        {
            return true;
        }
    }
    let predicates = |header: &ImplHeader, names: &ImplNames, free: &HashSet<String>| {
        header
            .generics
            .predicates
            .iter()
            .filter(|(left, bound)| !mentions(left, free) && !mentions(bound, free))
            .map(|(left, bound)| applied(&names.bounds, &predicate_tokens(left, bound)))
            .collect::<BTreeSet<_>>()
    };
    !predicates(new_header, new_names, &new_free)
        .is_subset(&predicates(old_header, old_names, &old_free))
}

/// `tokens` に名前の付け直し `names` を当てた字句から、省略できる末尾のカンマ (型の引数の `>` の前と、並びの
/// 最後の `,`) を除く。マクロの引数の中 (字面で照合される) のカンマと、`(u8,)` の 1 要素のタプルの印のカンマは
/// 残す。
fn applied(
    names: &super::rust_type_api::Renaming,
    tokens: &super::rust_type_api::Tokens,
) -> Vec<String> {
    use super::rust_type_api::Role;
    let mut out: Vec<String> = Vec::with_capacity(tokens.0.len());
    let mut removable = false;
    for (token, text) in tokens.0.iter().zip(names.apply(tokens)) {
        let opaque = token.role == Role::Opaque;
        if text == ">" && !opaque && removable {
            out.pop();
        }
        removable = text == "," && !opaque;
        out.push(text);
    }
    if removable {
        out.pop();
    }
    out
}

/// `items` の並べ替えを全て (今の並びから) `visit` に渡す (Heap の方法)。`visit` が真を返せば止める。
fn for_each_permutation(items: &mut [usize], mut visit: impl FnMut(&[usize]) -> bool) {
    let mut counters = vec![0usize; items.len()];
    if visit(items) {
        return;
    }
    let mut at = 1;
    while at < items.len() {
        if counters[at] < at {
            if at % 2 == 0 {
                items.swap(0, at);
            } else {
                items.swap(counters[at], at);
            }
            if visit(items) {
                return;
            }
            counters[at] += 1;
            at = 1;
        } else {
            counters[at] = 0;
            at += 1;
        }
    }
}

/// 項目を囲む inherent impl の見出しを旧新で比べた結果 (Issue #307)。
pub(crate) struct ImplHeaderChange {
    /// 見出しの変化。見出しの型の引数の名前の付け直しが旧新で違い、付け直したメンバーの signature が
    /// 違えば (読めなければ) 壊す。
    pub(crate) change: super::rust_type_api::ShapeChange,
    /// 見出しの型の引数の名前を付け直すと、メンバーの signature の契約の部分 (と定数の値) が同じ (signature
    /// の字面の違いは付け直しと整形だけ)。
    pub(crate) member_renamed: bool,
    /// 関連定数の値のマクロの引数の中 (字面のまま比べる) に現れる型の引数の対応が旧新で変わり、値が同じと
    /// 言えない (signature の字面が同じでも値の変更として比べる)。
    pub(crate) value_changed: bool,
    /// 旧新の見出しの字面 (表示用)。
    pub(crate) old_text: String,
    pub(crate) new_text: String,
}

impl ImplHeaderChange {
    /// 見出しを読めないときの結果 (壊す)。
    pub(crate) fn breaking() -> Self {
        Self {
            change: super::rust_type_api::ShapeChange::Breaking,
            member_renamed: false,
            value_changed: false,
            old_text: String::new(),
            new_text: String::new(),
        }
    }
}

/// 見出しとメンバー `member` の契約の部分の字句 (対象の型・境界・述語・メンバーの signature の切れ)。
fn contract_tokens<'a>(
    header: &'a ImplHeader,
    member: &'a Option<MemberSignature>,
) -> Vec<&'a super::rust_type_api::Tokens> {
    std::iter::once(&header.self_type)
        .chain(
            header
                .generics
                .params
                .iter()
                .flat_map(|param| &param.bounds),
        )
        .chain(
            header
                .generics
                .predicates
                .iter()
                .flat_map(|(left, bound)| [left, bound]),
        )
        .chain(member.iter().flat_map(|(pieces, ..)| pieces.iter()))
        .collect()
}

/// メンバー `member` (関連定数) の値の字句。
fn value_tokens(member: &Option<MemberSignature>) -> Vec<&super::rust_type_api::Tokens> {
    member
        .iter()
        .flat_map(|(_, value, _)| value.iter())
        .collect()
}

/// 型の引数の名前 (宣言の字面) から `r#` と寿命の `'` を外したもの (マクロの引数の中の字句と照らし合わせる)。
fn plain_name(name: &str) -> &str {
    super::rust_type_api::unraw(name.trim_start_matches('\''))
}

/// マクロの引数の中に現れる型の引数の名前: 寿命か (名前空間) と、[`plain_name`]。
type OpaqueName = (bool, String);

/// 型の引数 `param` が名前 `name` ([`OpaqueName`]) のものか。
fn is_named(param: &super::rust_type_api::Param, (lifetime, plain): &OpaqueName) -> bool {
    (param.kind == super::rust_type_api::ParamKind::Lifetime) == *lifetime
        && plain_name(&param.name) == plain.as_str()
}

/// `tokens` のマクロの引数の中 (字面のまま比べる) に現れる、`header` の型の引数の名前 ([`OpaqueName`])。マクロの
/// 引数の中の寿命は `'` と名前の 2 つの字句に分かれ、`r#` も残るので、それらを外して照らし合わせる (`'` が前に
/// あれば寿命の名前空間)。
fn opaque_params<'t>(
    header: &ImplHeader,
    tokens: impl IntoIterator<Item = &'t super::rust_type_api::Tokens>,
) -> std::collections::HashSet<OpaqueName> {
    use super::rust_type_api::Role;
    let mut found = std::collections::HashSet::new();
    for tokens in tokens {
        for (at, token) in tokens.0.iter().enumerate() {
            if token.role != Role::Opaque {
                continue;
            }
            let text = token.compared();
            let lifetime = text.starts_with('\'') || at > 0 && tokens.0[at - 1].compared() == "'";
            let name = (lifetime, plain_name(text).to_owned());
            if header
                .generics
                .params
                .iter()
                .any(|param| is_named(param, &name))
            {
                found.insert(name);
            }
        }
    }
    found
}

/// 付け直し `names` での、`header` の型の引数のうち名前が `name` ([`OpaqueName`]) のものの番号。
fn plain_number<'n>(
    names: &'n ImplNames,
    header: &ImplHeader,
    name: &OpaqueName,
) -> Option<&'n String> {
    let param = header
        .generics
        .params
        .iter()
        .find(|param| is_named(param, name))?;
    let renaming = &names.bounds;
    if param.name.starts_with('\'') {
        renaming.lifetimes.get(&param.name)
    } else {
        renaming
            .types
            .get(&param.name)
            .or_else(|| renaming.values.get(&param.name))
    }
}

/// `node` か、その中に種類が `kind` の節があるか。
fn contains_kind(node: tree_sitter::Node<'_>, kind: &str) -> bool {
    let mut stack = vec![node];
    while let Some(node) = stack.pop() {
        if node.kind() == kind {
            return true;
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    false
}

/// 述語 `left: bound` の字句 (左辺の `for<'a>` の束縛が境界にも及ぶよう、続けて付け直す)。
fn predicate_tokens(
    left: &super::rust_type_api::Tokens,
    bound: &super::rust_type_api::Tokens,
) -> super::rust_type_api::Tokens {
    use super::rust_type_api::{Role, Token, Tokens};
    let separator = Token {
        text: ":".to_owned(),
        spaced: false,
        role: Role::Other,
    };
    Tokens(
        left.0
            .iter()
            .cloned()
            .chain(std::iter::once(separator))
            .chain(bound.0.iter().cloned())
            .collect(),
    )
}

/// 名前の付け直しが同じか。
fn same_renaming(
    old: &super::rust_type_api::Renaming,
    new: &super::rust_type_api::Renaming,
) -> bool {
    old.types == new.types && old.values == new.values && old.lifetimes == new.lifetimes
}

/// `start` に始まる、impl の本体 (`declaration_list`) の直下の項目 (メソッド・関連定数)。
fn member_at(root: tree_sitter::Node<'_>, start: (usize, usize)) -> Option<tree_sitter::Node<'_>> {
    let point = tree_sitter::Point {
        row: start.0,
        column: start.1,
    };
    let mut node = root.descendant_for_point_range(point, point)?;
    loop {
        if node.start_position() == point
            && node
                .parent()
                .is_some_and(|body| body.kind() == "declaration_list")
        {
            return Some(node);
        }
        node = node.parent()?;
    }
}

/// メソッド・関連定数の signature の字句: 契約になる部分の切れ (関数は本体と引数のパターンを除く部分、
/// 定数は型) と、定数の値。
type MemberSignature = (
    Vec<super::rust_type_api::Tokens>,
    Option<super::rust_type_api::Tokens>,
    bool,
);

/// メソッド・関連定数の signature の字句 ([`MemberSignature`])。引数は型だけを見る (パターンの名前と束縛の
/// `mut` は契約でない)。読めなければ None。
fn member_signature(member: tree_sitter::Node<'_>, source: &[u8]) -> Option<MemberSignature> {
    use super::rust_type_api::Tokens;
    let comment =
        |node: tree_sitter::Node<'_>| matches!(node.kind(), "line_comment" | "block_comment");
    match member.kind() {
        "function_item" => {
            let body = member.child_by_field_name("body");
            // 戻り値が `impl Trait` (マクロが返すかもしれない型を含む) か `async` の関数は、隠れた型と外に漏れる
            // auto trait が本体で決まるので、本体も契約の部分にする。引数のパターンも本体の名前の型を決めるので、
            // 引数はそのまま比べる。`async` は修飾子の構文の字句で見る (コメントが隣り合っても)。本体が参照する
            // ほかの宣言の変更は比べない (Issue #323)。
            let mut cursor = member.walk();
            let hidden_return = member.child_by_field_name("return_type").is_some_and(|ty| {
                contains_kind(ty, "abstract_type") || contains_kind(ty, "macro_invocation")
            }) || member
                .children(&mut cursor)
                .any(|child| child.kind() == "function_modifiers" && contains_kind(child, "async"));
            let mut pieces = Vec::new();
            let mut cursor = member.walk();
            for child in member.children(&mut cursor) {
                if comment(child) {
                    continue;
                }
                if Some(child) == body {
                    if hidden_return {
                        pieces.push(Tokens::of(child, source)?);
                    }
                    continue;
                }
                if child.kind() != "parameters" {
                    pieces.push(Tokens::of(child, source)?);
                    continue;
                }
                if hidden_return {
                    // 引数をパターンも含めてそのまま比べる (省略できる最後のカンマは除く)。
                    let mut cursor = child.walk();
                    let mut parts: Vec<tree_sitter::Node<'_>> = child
                        .children(&mut cursor)
                        .filter(|part| !comment(*part))
                        .collect();
                    if parts.len() >= 2 && parts[parts.len() - 2].kind() == "," {
                        parts.remove(parts.len() - 2);
                    }
                    pieces.push(Tokens::of_nodes(&parts, source)?);
                    continue;
                }
                let mut cursor = child.walk();
                for parameter in child.named_children(&mut cursor) {
                    match parameter.kind() {
                        "parameter" => {
                            // `self: &Self` の `self` は受け手 (ほかの引数のパターンと違い契約の一部)。
                            if let Some(pattern) = parameter.child_by_field_name("pattern")
                                && pattern.kind() == "self"
                            {
                                pieces.push(Tokens::of(pattern, source)?);
                            }
                            pieces
                                .push(Tokens::of(parameter.child_by_field_name("type")?, source)?);
                        }
                        // `mut self` の `mut` は束縛の側。
                        "self_parameter" => {
                            let tokens = Tokens::of(parameter, source)?;
                            let start = usize::from(
                                tokens
                                    .0
                                    .first()
                                    .is_some_and(|token| token.compared() == "mut"),
                            );
                            pieces.push(Tokens(tokens.0[start..].to_vec()));
                        }
                        _ if comment(parameter) => {}
                        _ => pieces.push(Tokens::of(parameter, source)?),
                    }
                }
            }
            Some((pieces, None, hidden_return))
        }
        "const_item" => Some((
            vec![Tokens::of(member.child_by_field_name("type")?, source)?],
            Some(Tokens::of(member.child_by_field_name("value")?, source)?),
            false,
        )),
        _ => None,
    }
}

/// 旧新の宣言の有る条件が、どの設定でも同じく成り立つか (`#[cfg]` の書き方の違いは同じとみなす)。読めない
/// 条件があるか、原子が多すぎれば false。
pub(crate) fn same_availability(
    old: &RustDeclarationContext,
    new: &RustDeclarationContext,
) -> bool {
    use super::rust_cfg_predicate::{Atoms, Predicate};
    let (Some(old_attributes), Some(new_attributes)) = (&old.availability, &new.availability)
    else {
        return false;
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
        return false;
    };
    configs.all(|config| old_predicate.holds(config) == new_predicate.holds(config))
}

/// 対象の型の字句から、型の名前の前の経路 (`crate::S<T>` の `crate::`) を除く (同じ型の書き方の違い)。
pub(crate) fn last_segment(tokens: Vec<String>) -> Vec<String> {
    let arguments = tokens
        .iter()
        .position(|token| token == "<")
        .unwrap_or(tokens.len());
    match tokens[..arguments].iter().rposition(|token| token == "::") {
        Some(separator) => tokens[separator + 1..].to_vec(),
        None => tokens,
    }
}

/// impl の型の引数と `where` を比べる (Issue #307)。impl の見出しは項目を使える型の条件なので、構造体の
/// 型の引数 (暗黙の寿命の境界を保つ) とは違い、引数ごとの境界 (寿命も) と `where` の述語が旧い版の部分集合
/// なら緩めるだけ (互換)、等しければ同じ、それ以外は強める (壊す)。`?Sized` を足せば緩め、外せば強める。
/// 引数は対象の型に現れる順で組にし ([`ImplNames`])、数・種類・const の型が違えば壊す。
fn compare_impl_generics(
    old: &super::rust_type_api::Generics,
    new: &super::rust_type_api::Generics,
    old_names: &ImplNames,
    new_names: &ImplNames,
) -> super::rust_type_api::ShapeChange {
    use super::rust_type_api::{Param, Renaming, ShapeChange, Tokens};
    use std::collections::BTreeSet;
    fn contains<T: Ord>(old: &BTreeSet<T>, new: &BTreeSet<T>) -> ShapeChange {
        if old == new {
            ShapeChange::Unchanged
        } else if new.is_subset(old) {
            ShapeChange::Compatible
        } else {
            ShapeChange::Breaking
        }
    }
    // 使われない寿命の引数 (並びの後ろ) は、どの寿命でも満たせるので比べない。使われる引数の数が違えば壊す
    // (`T: 'a + 'b, 'a: 'b` から `T: 'b` のように、寿命の関係から同じになることは確かめない)。
    if old_names.used != new_names.used {
        return ShapeChange::Breaking;
    }
    let renamed = |names: &Renaming, tokens: &Option<Tokens>| {
        tokens.as_ref().map(|tokens| applied(names, tokens))
    };
    let bounds = |param: &Param, names: &Renaming| -> BTreeSet<Vec<String>> {
        param
            .bounds
            .iter()
            .map(|bound| applied(names, bound))
            .collect()
    };
    // 大きさの決まらない型を許す (`?Sized` で、`Sized` を書いていない)。
    let unsized_allowed = |param: &Param| param.relaxed && !param.sized_written;
    let mut change = ShapeChange::Unchanged;
    for (&old_index, &new_index) in old_names.order[..old_names.used]
        .iter()
        .zip(&new_names.order[..new_names.used])
    {
        let (old_param, new_param) = (&old.params[old_index], &new.params[new_index]);
        if old_param.kind != new_param.kind
            || renamed(&old_names.bounds, &old_param.const_type)
                != renamed(&new_names.bounds, &new_param.const_type)
        {
            return ShapeChange::Breaking;
        }
        change = change.max(contains(
            &bounds(old_param, &old_names.bounds),
            &bounds(new_param, &new_names.bounds),
        ));
        match (unsized_allowed(old_param), unsized_allowed(new_param)) {
            (true, false) => return ShapeChange::Breaking,
            (false, true) => change = change.max(ShapeChange::Compatible),
            _ => {}
        }
    }
    let predicates = |generics: &super::rust_type_api::Generics, names: &Renaming| {
        generics
            .predicates
            .iter()
            .map(|(left, bound)| applied(names, &predicate_tokens(left, bound)))
            .collect::<BTreeSet<_>>()
    };
    change.max(contains(
        &predicates(old, &old_names.bounds),
        &predicates(new, &new_names.bounds),
    ))
}

/// `start` に始まる項目を、本体 (`declaration_list`) の直下に持つ inherent impl (trait の impl は None)。
fn inherent_impl_at(
    root: tree_sitter::Node<'_>,
    start: (usize, usize),
) -> Option<tree_sitter::Node<'_>> {
    let point = tree_sitter::Point {
        row: start.0,
        column: start.1,
    };
    let mut node = root.descendant_for_point_range(point, point)?;
    loop {
        if node.start_position() == point
            && let Some(body) = node.parent()
            && body.kind() == "declaration_list"
            && let Some(container) = body.parent()
            && container.kind() == "impl_item"
        {
            return container
                .child_by_field_name("trait")
                .is_none()
                .then_some(container);
        }
        node = node.parent()?;
    }
}

/// inherent impl `item` の見出し (読めなければ None)。
fn impl_header(item: tree_sitter::Node<'_>, source: &[u8]) -> Option<ImplHeader> {
    let self_type = super::rust_type_api::Tokens::of(item.child_by_field_name("type")?, source)?;
    let mut cursor = item.walk();
    let where_clause = item
        .named_children(&mut cursor)
        .find(|child| child.kind() == "where_clause");
    let generics = super::rust_type_api::Generics::parse(
        item.child_by_field_name("type_parameters"),
        where_clause,
        source,
    )?;
    Some(ImplHeader {
        self_type,
        generics,
    })
}

/// impl `item` の見出しの字面 (本体の前まで。空白を 1 つに詰める)。
fn impl_header_text(item: tree_sitter::Node<'_>, source: &[u8]) -> String {
    let end = item
        .child_by_field_name("body")
        .map_or(item.end_byte(), |body| body.start_byte());
    String::from_utf8_lossy(&source[item.start_byte()..end])
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
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
