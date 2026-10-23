//! 型の移動と同じ差分でシグネチャの変わったメンバーを、移動元の削除と移動先の追加の組にする。

use std::collections::{HashMap, HashSet};

use tree_sitter::Node;

use super::{ApiRelationKey, ApiSymbolCandidate, PreparedDiffFile, api_relation_key, bare_name};
use crate::engine::parser;
use crate::models::review::MovedSymbol;

pub(super) type Symbols = [(String, String, String)];

/// Phase 0 で抽出したファイルごとの全 exported シンボル。旧側は旧パス、新側は新パスで引く。
/// 自由関数・定数の移動の組 (`moved_symbols`) も使う。
pub(crate) struct MemberFileFacts<'a> {
    pub(super) old: HashMap<&'a str, &'a Symbols>,
    /// 新側のシンボルと、そのファイルの中の呼び出し先。
    pub(super) new: HashMap<&'a str, (&'a Symbols, &'a HashSet<String>)>,
    /// 変更後も残るファイルの旧パス → 新パス (変更・rename)。
    pub(super) new_path_of: HashMap<&'a str, &'a str>,
    /// 変更前からあったファイルの新パス → 旧パス (変更・rename)。
    pub(super) old_path_of: HashMap<&'a str, &'a str>,
}

impl<'a> MemberFileFacts<'a> {
    pub(crate) fn new(
        diff_files: &'a [crate::models::impact::DiffFile],
        prepared: &'a [PreparedDiffFile],
    ) -> Self {
        let mut old = HashMap::new();
        let mut new = HashMap::new();
        let mut new_path_of = HashMap::new();
        let mut old_path_of = HashMap::new();
        for df in diff_files {
            if df.old_path != "/dev/null" && df.new_path != "/dev/null" {
                new_path_of.insert(df.old_path.as_str(), df.new_path.as_str());
                old_path_of.insert(df.new_path.as_str(), df.old_path.as_str());
            }
        }
        for (df, prep) in diff_files.iter().zip(prepared) {
            let (old_syms, new_syms) = match prep {
                PreparedDiffFile::Skip => continue,
                PreparedDiffFile::Added {
                    new_syms,
                    in_file_callees,
                } => (None, new_syms.as_deref().zip(Some(in_file_callees))),
                PreparedDiffFile::Deleted { old_syms, .. } => (old_syms.as_deref(), None),
                PreparedDiffFile::Modified {
                    old_syms,
                    new_syms,
                    in_file_callees,
                    ..
                } => (
                    old_syms.as_deref(),
                    new_syms.as_deref().zip(Some(in_file_callees)),
                ),
            };
            if let Some(syms) = old_syms {
                old.insert(df.old_path.as_str(), syms);
            }
            if let Some(facts) = new_syms {
                new.insert(df.new_path.as_str(), facts);
            }
        }
        Self {
            old,
            new,
            new_path_of,
            old_path_of,
        }
    }

    /// 組になり得るメンバー (別のファイルの旧側に同じ qualname・種別があり、シグネチャが違う
    /// 新側のメンバー) の bare 名を参照検索の対象に足す。変更ファイルのシグネチャ変更と同じく、
    /// 移動先のファイル内から呼ばれていても呼び出し側の追随を確かめられるようにする。
    pub(crate) fn collect_index_names(&self, index_names: &mut HashSet<String>) {
        let mut old_members: HashMap<(&str, &str), Vec<(&str, &str)>> = HashMap::new();
        for (path, syms) in &self.old {
            for (name, kind, sig) in syms.iter() {
                if name.contains('.') {
                    old_members
                        .entry((name.as_str(), kind.as_str()))
                        .or_default()
                        .push((*path, sig.as_str()));
                }
            }
        }
        if old_members.is_empty() {
            return;
        }
        for (path, (syms, _)) in &self.new {
            for (name, kind, sig) in syms.iter() {
                if old_members
                    .get(&(name.as_str(), kind.as_str()))
                    .is_some_and(|olds| {
                        olds.iter()
                            .any(|(old_path, old_sig)| old_path != path && *old_sig != sig.as_str())
                    })
                {
                    index_names.insert(bare_name(name).to_string());
                }
            }
        }
    }
}

/// メンバーを持つ型の種別。
fn is_owner_kind(kind: &str) -> bool {
    matches!(kind, "class" | "struct" | "trait" | "interface" | "enum")
}

/// 宣言を読む側。移動元は base の旧版、移動先は作業ツリーの新版。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Side {
    Old,
    New,
}

/// Rust の `#[cfg(...)]` / `#[cfg_attr(...)]` が、node 自身か、node を囲むモジュール・ファイルに
/// 掛かっているか (外側の属性、impl や mod の本体の内側の属性、ファイル直下の `#![cfg(...)]`)。
/// 属性名は構文木で見る (属性の中のコメントや、`#[doc = "cfg(x)"]` の文字列に左右されない)。
/// 属性の形を読めなければ cfg とみなす。
pub(super) fn rust_cfg_applies(node: Node<'_>, source: &[u8]) -> bool {
    let is_cfg = |attribute_item: Node<'_>| {
        let mut cursor = attribute_item.walk();
        let Some(attribute) = attribute_item
            .named_children(&mut cursor)
            .find(|child| child.kind() == "attribute")
        else {
            return true;
        };
        let name = match attribute.named_child(0) {
            Some(path) if path.kind() == "identifier" => Some(path),
            Some(path) if path.kind() == "scoped_identifier" => path.child_by_field_name("name"),
            _ => None,
        };
        name.is_none_or(|name| matches!(rust_name(name, source), Some("cfg" | "cfg_attr")))
    };
    let inner_cfg = |container: Node<'_>| {
        let mut cursor = container.walk();
        container
            .named_children(&mut cursor)
            .any(|child| child.kind() == "inner_attribute_item" && is_cfg(child))
    };
    let mut current = Some(node);
    while let Some(node) = current {
        let mut previous = node.prev_named_sibling();
        while let Some(sibling) = previous {
            match sibling.kind() {
                "attribute_item" if is_cfg(sibling) => return true,
                "attribute_item" | "line_comment" | "block_comment" => {
                    previous = sibling.prev_named_sibling();
                }
                _ => break,
            }
        }
        if node.child_by_field_name("body").is_some_and(inner_cfg)
            || (node.kind() == "source_file" && inner_cfg(node))
        {
            return true;
        }
        current = node.parent();
    }
    false
}

/// Rust の識別子の文字列 (`r#` を外す)。
fn rust_name<'s>(node: Node<'_>, source: &'s [u8]) -> Option<&'s str> {
    let text = node.utf8_text(source).ok()?;
    Some(text.strip_prefix("r#").unwrap_or(text))
}

/// `source` の中で、`<owner>.<member>` が owner の型宣言に属すると証明できるか。
///
/// qualname の owner は型の名前だけなので、名前だけでは別の型のメンバーと区別できない
/// (別モジュールの同名の型、`use ... as Owner` や型別名に付けた impl、クラス式)。次をすべて
/// 満たすときだけ証明できたとする:
/// - 非公開・入れ子を含む全宣言に、owner と同じ名前の型 (class / struct / trait / interface /
///   enum) が 1 件、member と同じ bare 名の宣言が 1 件だけ (別の型の同名メソッド・同名の
///   関数・cfg で分けた 2 定義があれば、どの宣言どうしが対応するか決まらない)
/// - その宣言が `<owner>.<member>` の関数・メソッドで、型宣言の本体に直接置かれている
///   (本体の中のクラス式やオブジェクトリテラルのメソッドは別のものに属する)。Rust では、
///   型宣言と同じスコープにある inherent impl (`impl Owner` / `impl<T> Owner<T>`) の本体でも
///   よい。同じスコープに同名の `use` や型別名は置けないので、impl の型は型宣言を指す
/// - Rust では、型宣言・impl・メンバーに `cfg` / `cfg_attr` が無く、そのスコープに owner の
///   名前を束縛し得る `use`・型別名・`extern crate` や、項目を生成し得るマクロ呼び出し
///   (`include!` など) が無い (無効化された型宣言の代わりに別名が impl の型になり得る)
///
/// Go のレシーバ・Swift の extension・C++ のクラス外定義のように型の外で宣言したメンバー、
/// trait impl、言語を判定できない・構文エラーがあるファイルは証明しない。
pub(crate) fn proves_member_binding(path: &str, source: &[u8], owner: &str, member: &str) -> bool {
    use crate::language::LangId;
    use tree_sitter::Point;

    // 型宣言の本体に直接置かれたメンバーか。本体の包み (class_body・block・Java の
    // enum_body_declarations・Python の decorated_definition など) だけを上り、最初に出会った
    // 包みでないノードが型宣言そのものであることを求める (クラス式・オブジェクトリテラル・
    // 入れ子の関数や条件分岐の中のメンバーは別のものに属する)。
    fn direct_member(member: Node<'_>, owner: Node<'_>) -> bool {
        // Ruby の `def Other.pack` は本体に置いても別のオブジェクトの特異メソッド。受け手が
        // 型と同じ名前の定数でも、本体の中の同名の定数を指し得るので `self` だけを認める。
        if member.kind() == "singleton_method"
            && !member
                .named_child(0)
                .is_some_and(|receiver| receiver.kind() == "self")
        {
            return false;
        }
        let is_wrapper = |kind: &str| {
            kind.ends_with("_body")
                || matches!(
                    kind,
                    "body"
                        | "block"
                        | "declaration_list"
                        | "field_declaration_list"
                        | "body_statement"
                        | "enum_body_declarations"
                        | "object_type"
                        | "decorated_definition"
                )
        };
        let mut node = member;
        while let Some(parent) = node.parent() {
            if !is_wrapper(parent.kind()) {
                return parent.id() == owner.id();
            }
            node = parent;
        }
        false
    }

    // use tree が束縛する名前を `out` に積む。パスは解決せず、末尾の名前か別名だけを見る
    // (`a::{self}` は `a`)。glob は名前を束縛しないので積まない。読めない形は None。
    fn use_tree_bindings(
        node: Node<'_>,
        source: &[u8],
        parent: Option<&str>,
        out: &mut Vec<String>,
    ) -> Option<()> {
        match node.kind() {
            "identifier" => out.push(rust_name(node, source)?.to_owned()),
            "self" => out.push(parent?.to_owned()),
            "scoped_identifier" => {
                out.push(rust_name(node.child_by_field_name("name")?, source)?.to_owned());
            }
            "use_as_clause" => {
                out.push(rust_name(node.child_by_field_name("alias")?, source)?.to_owned());
            }
            "scoped_use_list" => {
                let parent = node
                    .child_by_field_name("path")
                    .and_then(|path| match path.kind() {
                        "identifier" => rust_name(path, source),
                        "scoped_identifier" => rust_name(path.child_by_field_name("name")?, source),
                        _ => None,
                    });
                use_tree_bindings(node.child_by_field_name("list")?, source, parent, out)?;
            }
            "use_list" => {
                let mut cursor = node.walk();
                for item in node.named_children(&mut cursor) {
                    if !matches!(item.kind(), "line_comment" | "block_comment") {
                        use_tree_bindings(item, source, parent, out)?;
                    }
                }
            }
            "use_wildcard" => {}
            _ => return None,
        }
        Some(())
    }

    // Rust のスコープに、owner の名前を束縛する項目 (`use`・型別名・`extern crate`) や、項目を
    // 生成し得るマクロ呼び出しがあるか。束縛する名前だけを見る (`type Shared = Arc<Owner>;` や
    // `use x::y as Z;` は owner を束縛しない)。glob の `use` は同じスコープの型宣言を隠せない。
    fn scope_may_rebind(scope: Node<'_>, source: &[u8], owner: &str) -> bool {
        let mut cursor = scope.walk();
        scope
            .named_children(&mut cursor)
            .any(|item| match item.kind() {
                "use_declaration" => {
                    let mut bindings = Vec::new();
                    item.child_by_field_name("argument")
                        .and_then(|argument| {
                            use_tree_bindings(argument, source, None, &mut bindings)
                        })
                        .is_none()
                        || bindings.iter().any(|name| name == owner)
                }
                "type_item" | "extern_crate_declaration" => item
                    .child_by_field_name("alias")
                    .or_else(|| item.child_by_field_name("name"))
                    .is_none_or(|name| rust_name(name, source) == Some(owner)),
                "macro_invocation" => true,
                "expression_statement" => item
                    .named_child(0)
                    .is_some_and(|child| child.kind() == "macro_invocation"),
                _ => false,
            })
    }

    let proof = || -> Option<bool> {
        let lang = parser::detect_lang(camino::Utf8Path::new(path), source).ok()?;
        let tree = parser::parse_source(source, lang).ok()?;
        let root = tree.root_node();
        if !super::rust_ref_sites::clean_tree(root) {
            return None;
        }
        let symbols = crate::engine::symbols::extract_symbols(root, source, lang).ok()?;
        let declarations = super::unfiltered_declaration_signatures(&symbols, root, source, lang);
        let mut owners = symbols
            .iter()
            .zip(&declarations)
            .filter(|(_, (name, kind, _))| name == owner && is_owner_kind(kind));
        let mut members = symbols
            .iter()
            .zip(&declarations)
            .filter(|(_, (name, _, _))| bare_name(name) == member);
        let (Some((owner_symbol, _)), None, Some((member_symbol, (qualname, kind, _))), None) =
            (owners.next(), owners.next(), members.next(), members.next())
        else {
            return Some(false);
        };
        if qualname.as_str() != format!("{owner}.{member}")
            || !matches!(kind.as_str(), "function" | "method")
        {
            return Some(false);
        }
        let node = |range: &crate::models::location::Range| {
            root.descendant_for_point_range(
                Point::new(range.start.line, range.start.column),
                Point::new(range.end.line, range.end.column),
            )
        };
        let owner_node = node(&owner_symbol.range)?;
        let member_node = node(&member_symbol.range)?;
        let rust = lang == LangId::Rust;
        if rust && (rust_cfg_applies(owner_node, source) || rust_cfg_applies(member_node, source)) {
            return Some(false);
        }
        // 型宣言の本体に直接書いたメンバー (class / trait などの本体に書く言語)
        if direct_member(member_node, owner_node) {
            return Some(true);
        }
        if !rust {
            return Some(false);
        }
        let body = member_node.parent()?;
        let impl_item = body.parent()?;
        if member_node.kind() != "function_item"
            || body.kind() != "declaration_list"
            || impl_item.kind() != "impl_item"
            || impl_item.child_by_field_name("trait").is_some()
            || rust_cfg_applies(impl_item, source)
        {
            return Some(false);
        }
        let mut target = impl_item.child_by_field_name("type")?;
        if target.kind() == "generic_type" {
            target = target.child_by_field_name("type")?;
        }
        let scope = owner_node.parent()?;
        Some(
            target.kind() == "type_identifier"
                && target.utf8_text(source).ok()? == owner
                && impl_item.parent()?.id() == scope.id()
                && !scope_may_rebind(scope, source, owner),
        )
    };
    proof().unwrap_or(false)
}

/// 型の移動と同じ差分でシグネチャの変わったメンバー 1 組。
pub(crate) struct MovedMemberChange<'a> {
    /// 移動元のメンバー (削除候補)。
    pub(crate) old: ApiSymbolCandidate,
    /// 移動先のメンバー (追加候補)。
    pub(crate) new: ApiSymbolCandidate,
    /// 移動先ファイルの中の呼び出し先 (Phase 0 の抽出結果)。
    pub(crate) new_in_file_callees: &'a HashSet<String>,
}

/// 型 (`moved` の owner) を別のファイルへ移した同じ差分で、その型のメンバーのシグネチャを
/// 変えた削除と追加を組にする。組にした削除と追加は `removed` / `added` から外して返す。
///
/// `reconcile_with_moves` は (name, kind, signature) の厳密一致だけを move にするので、
/// 引数を足したメソッドは移動元の削除と移動先の追加に割れ、呼び出しを直していても名前の
/// 参照が残るため api.rm になる。git が rename として渡す差分なら、同じ変更は変更ファイルの
/// シグネチャ変更 (mod / mod_closed) になる。owner の移動を対応の証拠にして、組を同じ規則で
/// 判定できるようにする (Issue #109)。シグネチャの違う組は `moved` にしない (`moved` は
/// 非 blocking なので、移動と証明できていない変更を隠してしまう)。
///
/// 組にするのは次をすべて満たすときだけで、どれかが崩れたら従来どおりの削除と追加に残す:
/// - owner が型 (class / struct / trait / interface / enum) で、同じ (名前, 移動元) の move が 1 件
/// - 削除はメンバー (`<owner>.<member>` の関数・メソッド) で、削除の (name, kind, signature) が
///   対応付け不能 (`ambiguous_relation_keys`) でない
/// - 移動先の新規候補に同じ name・kind のものが 1 件だけあり、シグネチャが違い、厳密一致の
///   move にも別の削除との組にも使われていない
/// - 移動元と移動先のどちらでも、メンバーが owner の型宣言に属すると証明できる
///   (`binds(side, path, owner, member)`。[`proves_member_binding`] を参照)。解析は組の候補に
///   なったファイルだけで行う
pub(crate) fn pair_moved_member_changes<'a>(
    moved: &[MovedSymbol],
    added: Vec<ApiSymbolCandidate>,
    removed: Vec<ApiSymbolCandidate>,
    all_new_candidates: &[ApiSymbolCandidate],
    ambiguous_relation_keys: &HashSet<ApiRelationKey>,
    files: &MemberFileFacts<'a>,
    binds: &mut dyn FnMut(Side, &str, &str, &str) -> bool,
) -> (
    Vec<ApiSymbolCandidate>,
    Vec<ApiSymbolCandidate>,
    Vec<MovedMemberChange<'a>>,
) {
    // 同じ (名前, 移動元) の owner の move が複数あれば、どこへ移したか決められない。
    let mut owner_moves: HashMap<(&str, &str), Option<&str>> = HashMap::new();
    for m in moved {
        if is_owner_kind(&m.kind) {
            owner_moves
                .entry((m.name.as_str(), m.from.as_str()))
                .and_modify(|to| *to = None)
                .or_insert(Some(m.to.as_str()));
        }
    }
    if owner_moves.is_empty() {
        return (added, removed, Vec::new());
    }
    let moved_targets: HashSet<(&str, &str, &str)> = moved
        .iter()
        .map(|m| (m.name.as_str(), m.kind.as_str(), m.to.as_str()))
        .collect();
    // 削除の index → 移動先の新規候補の index。削除の並び順で積む。
    let mut proposals: Vec<(usize, usize)> = Vec::new();
    for (removed_ix, old) in removed.iter().enumerate() {
        if !matches!(old.kind.as_str(), "function" | "method") {
            continue;
        }
        let Some((owner, member)) = old.name.rsplit_once('.') else {
            continue;
        };
        if owner.is_empty() || member.is_empty() {
            continue;
        }
        let Some(&Some(to)) = owner_moves.get(&(owner, old.file.as_str())) else {
            continue;
        };
        if ambiguous_relation_keys.contains(&api_relation_key(old))
            || moved_targets.contains(&(old.name.as_str(), old.kind.as_str(), to))
            || !files.old.contains_key(old.file.as_str())
            || !files.new.contains_key(to)
        {
            continue;
        }
        let mut targets = all_new_candidates
            .iter()
            .enumerate()
            .filter(|(_, new)| new.file == to && new.name == old.name && new.kind == old.kind);
        let (Some((new_ix, new)), None) = (targets.next(), targets.next()) else {
            continue;
        };
        if new.signature == old.signature {
            continue;
        }
        if binds(Side::Old, &old.file, owner, member) && binds(Side::New, to, owner, member) {
            proposals.push((removed_ix, new_ix));
        }
    }

    // 1 件の新規候補に複数の削除が向いたら、どれとも組にしない。
    let mut uses: HashMap<usize, usize> = HashMap::new();
    for &(_, new_ix) in &proposals {
        *uses.entry(new_ix).or_default() += 1;
    }
    let pairs: HashMap<usize, usize> = proposals
        .into_iter()
        .filter(|(_, new_ix)| uses[new_ix] == 1)
        .collect();
    if pairs.is_empty() {
        return (added, removed, Vec::new());
    }

    let paired_new: HashSet<(&str, &str, &str, &str)> = pairs
        .values()
        .map(|&new_ix| {
            let new = &all_new_candidates[new_ix];
            (
                new.name.as_str(),
                new.kind.as_str(),
                new.file.as_str(),
                new.signature.as_str(),
            )
        })
        .collect();
    let kept_added = added
        .into_iter()
        .filter(|a| {
            !paired_new.contains(&(
                a.name.as_str(),
                a.kind.as_str(),
                a.file.as_str(),
                a.signature.as_str(),
            ))
        })
        .collect();
    let mut kept_removed = Vec::new();
    let mut changes = Vec::new();
    for (removed_ix, old) in removed.into_iter().enumerate() {
        let Some(&new_ix) = pairs.get(&removed_ix) else {
            kept_removed.push(old);
            continue;
        };
        let new = all_new_candidates[new_ix].clone();
        let new_in_file_callees = files.new[new.file.as_str()].1;
        changes.push(MovedMemberChange {
            old,
            new,
            new_in_file_callees,
        });
    }
    (kept_added, kept_removed, changes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::impact::DiffFile;

    const OLD: &str = "src/old.rs";
    const NEW: &str = "src/new.rs";
    const OLD_SIG: &str = "pub fn pack(&self, n: u32) -> u32";
    const NEW_SIG: &str = "pub fn pack(&self, n: u32, r: u32) -> u32";

    fn symbol(name: &str, kind: &str, signature: &str) -> (String, String, String) {
        (name.to_owned(), kind.to_owned(), signature.to_owned())
    }

    fn candidate(name: &str, kind: &str, file: &str, signature: &str) -> ApiSymbolCandidate {
        ApiSymbolCandidate {
            name: name.to_owned(),
            kind: kind.to_owned(),
            file: file.to_owned(),
            signature: signature.to_owned(),
        }
    }

    fn moved(name: &str, kind: &str, from: &str, to: &str) -> MovedSymbol {
        MovedSymbol {
            name: name.to_owned(),
            kind: kind.to_owned(),
            from: from.to_owned(),
            to: to.to_owned(),
        }
    }

    fn diff(old_path: &str, new_path: &str) -> DiffFile {
        DiffFile {
            old_path: old_path.to_owned(),
            new_path: new_path.to_owned(),
            hunks: Vec::new(),
            deleted_old_source: None,
        }
    }

    /// `Packer` を OLD から NEW へ移し、`Packer.pack` に引数を足した差分の材料。
    struct Scenario {
        old_syms: Vec<(String, String, String)>,
        new_syms: Vec<(String, String, String)>,
        /// 移動元・移動先でメンバーが owner に属すると証明できるか (`proves_member_binding`)。
        binds: [bool; 2],
        moved: Vec<MovedSymbol>,
        removed: Vec<ApiSymbolCandidate>,
        all_new: Vec<ApiSymbolCandidate>,
        ambiguous: HashSet<ApiRelationKey>,
    }

    impl Scenario {
        fn new() -> Self {
            Self {
                old_syms: vec![
                    symbol("Packer", "struct", "pub struct Packer"),
                    symbol("Packer.pack", "function", OLD_SIG),
                ],
                new_syms: vec![
                    symbol("Packer", "struct", "pub struct Packer"),
                    symbol("Packer.pack", "function", NEW_SIG),
                ],
                binds: [true, true],
                moved: vec![moved("Packer", "struct", OLD, NEW)],
                removed: vec![candidate("Packer.pack", "function", OLD, OLD_SIG)],
                all_new: vec![
                    candidate("Packer", "struct", NEW, "pub struct Packer"),
                    candidate("Packer.pack", "function", NEW, NEW_SIG),
                ],
                ambiguous: HashSet::new(),
            }
        }

        /// (残った追加の件数, 残った削除の件数, 組にした (旧 qualname, 新 signature))
        fn pair(&self) -> (usize, usize, Vec<(String, String)>) {
            let diff_files = [diff(OLD, "/dev/null"), diff("/dev/null", NEW)];
            let prepared = [
                PreparedDiffFile::Deleted {
                    old_syms: Some(self.old_syms.clone()),
                    old_source_unverified: false,
                },
                PreparedDiffFile::Added {
                    new_syms: Some(self.new_syms.clone()),
                    in_file_callees: HashSet::new(),
                },
            ];
            let files = MemberFileFacts::new(&diff_files, &prepared);
            let added: Vec<_> = self
                .all_new
                .iter()
                .filter(|c| c.name.contains('.'))
                .cloned()
                .collect();
            let (kept_added, kept_removed, changes) = pair_moved_member_changes(
                &self.moved,
                added,
                self.removed.clone(),
                &self.all_new,
                &self.ambiguous,
                &files,
                &mut |side, path, owner, member| {
                    assert_eq!((owner, member), ("Packer", "pack"));
                    match (side, path) {
                        (Side::Old, OLD) => self.binds[0],
                        (Side::New, NEW) => self.binds[1],
                        _ => panic!("{side:?} {path}"),
                    }
                },
            );
            (
                kept_added.len(),
                kept_removed.len(),
                changes
                    .into_iter()
                    .map(|change| (change.old.name, change.new.signature))
                    .collect(),
            )
        }
    }

    #[test]
    fn moved_member_pairs_a_unique_member_of_a_moved_type() {
        assert_eq!(
            Scenario::new().pair(),
            (0, 0, vec![("Packer.pack".to_owned(), NEW_SIG.to_owned())])
        );
    }

    #[test]
    fn moved_member_rejects_every_ambiguous_correspondence() {
        type Edit = fn(&mut Scenario);
        let cases: [(&str, Edit); 9] = [
            ("owner が型でない", |s| {
                s.moved = vec![moved("Packer", "function", OLD, NEW)];
            }),
            (
                "移動元でメンバーが owner に属すると証明できない",
                |s| {
                    s.binds[0] = false;
                },
            ),
            (
                "移動先でメンバーが owner に属すると証明できない",
                |s| {
                    s.binds[1] = false;
                },
            ),
            (
                "同じ (名前, 移動元) の owner の move が 2 件",
                |s| {
                    s.moved.push(moved("Packer", "class", OLD, "src/other.rs"));
                },
            ),
            ("削除の key が対応付け不能", |s| {
                s.ambiguous.insert((
                    "Packer.pack".to_owned(),
                    "function".to_owned(),
                    OLD_SIG.to_owned(),
                ));
            }),
            (
                "移動先の候補が別の厳密一致の move に使われている",
                |s| {
                    s.moved
                        .push(moved("Packer.pack", "function", "src/third.rs", NEW));
                },
            ),
            ("移動先の候補が 2 件", |s| {
                s.all_new.push(candidate(
                    "Packer.pack",
                    "function",
                    NEW,
                    "pub fn pack(&self) -> u32",
                ));
            }),
            ("シグネチャが同じ", |s| {
                s.all_new[1].signature = OLD_SIG.to_owned();
            }),
            ("削除がメンバーでない", |s| {
                s.removed = vec![candidate("pack", "function", OLD, OLD_SIG)];
            }),
        ];
        for (label, edit) in cases {
            let mut scenario = Scenario::new();
            edit(&mut scenario);
            let (_, kept_removed, changes) = scenario.pair();
            assert!(changes.is_empty(), "{label}: {changes:?}");
            assert_eq!(kept_removed, scenario.removed.len(), "{label}");
        }
    }

    /// 1 件の新規候補に 2 件の削除が向いたら、どちらも組にしない (先着順で決めない)。
    #[test]
    fn moved_member_rejects_two_removals_for_one_destination() {
        let second = "src/second.rs";
        let diff_files = [
            diff(OLD, "/dev/null"),
            diff(second, "/dev/null"),
            diff("/dev/null", NEW),
        ];
        let old_syms = vec![
            symbol("Packer", "struct", "pub struct Packer"),
            symbol("Packer.pack", "function", OLD_SIG),
        ];
        let prepared = [
            PreparedDiffFile::Deleted {
                old_syms: Some(old_syms.clone()),
                old_source_unverified: false,
            },
            PreparedDiffFile::Deleted {
                old_syms: Some(old_syms),
                old_source_unverified: false,
            },
            PreparedDiffFile::Added {
                new_syms: Some(vec![
                    symbol("Packer", "struct", "pub struct Packer"),
                    symbol("Packer.pack", "function", NEW_SIG),
                ]),
                in_file_callees: HashSet::new(),
            },
        ];
        let files = MemberFileFacts::new(&diff_files, &prepared);
        let all_new = vec![candidate("Packer.pack", "function", NEW, NEW_SIG)];
        let mut asked = Vec::new();
        let (_, kept_removed, changes) = pair_moved_member_changes(
            &[
                moved("Packer", "struct", OLD, NEW),
                moved("Packer", "struct", second, NEW),
            ],
            all_new.clone(),
            vec![
                candidate("Packer.pack", "function", OLD, OLD_SIG),
                candidate("Packer.pack", "function", second, OLD_SIG),
            ],
            &all_new,
            &HashSet::new(),
            &files,
            &mut |side, path, _, _| {
                asked.push((side, path.to_owned()));
                true
            },
        );
        assert!(changes.is_empty());
        assert_eq!(kept_removed.len(), 2);
        // 宣言は組の候補になったファイルだけ読む。
        assert!(
            asked
                .iter()
                .all(|(_, path)| [OLD, second, NEW].contains(&path.as_str())),
            "{asked:?}"
        );
    }

    /// 組になり得るメンバーだけを参照検索の対象に足す。
    #[test]
    fn moved_member_index_names_cover_only_possible_pairs() {
        let diff_files = [
            diff(OLD, "/dev/null"),
            diff("/dev/null", NEW),
            diff("src/same.rs", "src/same.rs"),
        ];
        let prepared = [
            PreparedDiffFile::Deleted {
                old_syms: Some(vec![
                    symbol("Packer.pack", "function", OLD_SIG),
                    symbol("Packer.keep", "function", "pub fn keep(&self)"),
                    symbol("free", "function", "pub fn free()"),
                ]),
                old_source_unverified: false,
            },
            PreparedDiffFile::Added {
                new_syms: Some(vec![
                    symbol("Packer.pack", "function", NEW_SIG),
                    symbol("Packer.keep", "function", "pub fn keep(&self)"),
                    symbol("free", "function", "pub fn free(n: u32)"),
                ]),
                in_file_callees: HashSet::new(),
            },
            PreparedDiffFile::Modified {
                unparsed_declarations: Vec::new(),
                old_syms: Some(vec![symbol("Local.run", "function", "pub fn run()")]),
                new_syms: Some(vec![symbol("Local.run", "function", "pub fn run(n: u32)")]),
                parameter_pattern_only_names: HashSet::new(),
                rust_const_additions: HashSet::new(),
                in_file_callees: HashSet::new(),
                new_export_surface_names: HashSet::new(),
            },
        ];
        let mut names = HashSet::new();
        MemberFileFacts::new(&diff_files, &prepared).collect_index_names(&mut names);
        assert_eq!(names, HashSet::from(["pack".to_owned()]));
    }

    /// メンバーが owner の型宣言に属すると、構文で証明できるときだけ組にする。
    #[test]
    fn moved_member_binding_requires_the_member_to_belong_to_the_owner() {
        for (path, source, expected) in [
            (
                "a.rs",
                "pub struct Packer;\nimpl Packer {\n    pub fn pack(&self) {}\n}\n",
                true,
            ),
            (
                "a.rs",
                "pub struct Packer<T>(T);\nimpl<T> Packer<T> {\n    pub fn pack(&self) {}\n}\n",
                true,
            ),
            (
                "a.rs",
                "pub mod m {\n    pub struct Packer;\n    impl Packer {\n        pub fn pack(&self) {}\n    }\n}\n",
                true,
            ),
            ("a.rs", "pub trait Packer {\n    fn pack(&self);\n}\n", true),
            (
                "a.py",
                "class Packer:\n    def pack(self):\n        pass\n",
                true,
            ),
            (
                "a.ts",
                "export class Packer {\n  pack(): void {}\n}\n",
                true,
            ),
            (
                "a.py",
                "class Packer:\n    @staticmethod\n    def pack():\n        pass\n",
                true,
            ),
            (
                "a.rs",
                "use std::fmt;\n#[derive(Debug)]\npub struct Packer;\n#[allow(dead_code)]\nimpl Packer {\n    #[inline]\n    pub fn pack(&self) {}\n}\n",
                true,
            ),
            // owner を束縛しない別名や use、cfg を含まない属性は妨げない
            (
                "a.rs",
                "use super::helpers;\nuse self::Packer as Alias;\n#[doc = \"cfg(x)\"]\npub struct Packer;\ntype SharedPacker = std::sync::Arc<Packer>;\nimpl Packer {\n    pub fn pack(&self) {}\n}\n",
                true,
            ),
            (
                "a.rs",
                "use {super::helpers, std::sync::Arc};\nuse std::io::{self, Read};\npub struct Packer;\nimpl Packer {\n    pub fn pack(&self) {}\n}\n",
                true,
            ),
            ("A.java", "enum Packer {\n  A;\n  void pack() {}\n}\n", true),
            ("a.rb", "class Packer\n  def pack(x)\n  end\nend\n", true),
            (
                "a.rb",
                "class Packer\n  def self.pack(x)\n  end\nend\n",
                true,
            ),
            // 同じスコープで owner を束縛する use、コメントを挟んだ cfg、別オブジェクトの特異メソッド
            (
                "a.rs",
                "use crate::other::Packer;\npub struct Packer;\nimpl Packer {\n    pub fn pack(&self) {}\n}\n",
                false,
            ),
            (
                "a.rs",
                "#[cfg/* c */(any())]\npub struct Packer;\nimpl Packer {\n    pub fn pack(&self) {}\n}\n",
                false,
            ),
            (
                "a.rb",
                "class Packer\n  def Other.pack(x)\n  end\nend\n",
                false,
            ),
            (
                "a.rb",
                "class Packer\n  Packer = Object.new\n  def Packer.pack(x)\n  end\nend\n",
                false,
            ),
            (
                "a.rs",
                "use crate::other::{Packer, Helper};\npub struct Packer;\nimpl Packer {\n    pub fn pack(&self) {}\n}\n",
                false,
            ),
            (
                "a.rs",
                "pub use crate::other::*;\n#[r#cfg(any())]\npub struct Packer;\nimpl Packer {\n    pub fn pack(&self) {}\n}\n",
                false,
            ),
            // ファイル全体・囲むモジュールの cfg
            (
                "a.rs",
                "#![cfg(any())]\npub struct Packer;\nimpl Packer {\n    pub fn pack(&self) {}\n}\n",
                false,
            ),
            (
                "a.rs",
                "#[cfg(unix)]\npub mod backend {\n    pub struct Packer;\n    impl Packer {\n        pub fn pack(&self) {}\n    }\n}\n",
                false,
            ),
            (
                "a.rs",
                "pub mod backend {\n    #![cfg(unix)]\n    pub struct Packer;\n    impl Packer {\n        pub fn pack(&self) {}\n    }\n}\n",
                false,
            ),
            (
                "a.rs",
                "#![allow(dead_code)]\npub struct Packer;\nimpl Packer {\n    pub fn pack(&self) {}\n}\n",
                true,
            ),
            // 本体の中のクラス式・オブジェクトリテラルのメソッドは別のものに属する
            (
                "a.ts",
                "export class Packer {\n  nested = class Packer {\n    pack(): void {}\n  };\n}\n",
                false,
            ),
            (
                "a.ts",
                "export class Packer {\n  helpers = {\n    pack(): void {},\n  };\n}\n",
                false,
            ),
            // cfg で無効にし得る型宣言・impl・メンバー、項目を生成し得るマクロ呼び出し
            (
                "a.rs",
                "#[cfg(any())]\npub struct Packer;\n\npub use crate::Other as Packer;\n\nimpl Packer {\n    pub fn pack(&self) {}\n}\n",
                false,
            ),
            (
                "a.rs",
                "pub struct Packer;\n#[cfg(test)]\nimpl Packer {\n    pub fn pack(&self) {}\n}\n",
                false,
            ),
            (
                "a.rs",
                "pub struct Packer;\nimpl Packer {\n    #[cfg_attr(test, allow(unused))]\n    pub fn pack(&self) {}\n}\n",
                false,
            ),
            (
                "a.rs",
                "pub struct Packer;\ninclude!(\"generated.rs\");\nimpl Packer {\n    pub fn pack(&self) {}\n}\n",
                false,
            ),
            // 別モジュールで別名を付けた別の型の impl (`use` と型別名)
            (
                "a.rs",
                "pub struct Packer;\nmod other {\n    use crate::Other as Packer;\n    impl Packer {\n        pub fn pack(&self) {}\n    }\n}\n",
                false,
            ),
            (
                "a.rs",
                "pub struct Packer;\nmod other {\n    type Packer = crate::Other;\n    impl Packer {\n        pub fn pack(&self) {}\n    }\n}\n",
                false,
            ),
            // 同じ名前の型が別モジュールにもある (非公開でも数える)
            (
                "a.rs",
                "pub struct Packer;\nimpl Packer {\n    pub fn pack(&self) {}\n}\nmod other {\n    struct Packer;\n}\n",
                false,
            ),
            // 別の型の同名メソッド・同名の関数・cfg で分けた 2 定義
            (
                "a.rs",
                "pub struct Packer;\nimpl Packer {\n    pub fn pack(&self) {}\n}\npub struct Other;\nimpl Other {\n    pub fn pack(&self) {}\n}\n",
                false,
            ),
            (
                "a.rs",
                "pub struct Packer;\nimpl Packer {\n    pub fn pack(&self) {}\n}\npub fn pack() {}\n",
                false,
            ),
            (
                "a.rs",
                "pub struct Packer;\nimpl Packer {\n    #[cfg(unix)]\n    pub fn pack(&self) {}\n    #[cfg(not(unix))]\n    pub fn pack(&self) {}\n}\n",
                false,
            ),
            // trait impl のメソッド、型宣言と違うスコープの impl、構文エラー
            (
                "a.rs",
                "pub struct Packer;\nimpl Runner for Packer {\n    fn pack(&self) {}\n}\n",
                false,
            ),
            (
                "a.rs",
                "pub struct Packer;\npub fn build() {\n    impl Packer {\n        pub fn pack(&self) {}\n    }\n}\n",
                false,
            ),
            (
                "a.rs",
                "pub struct Packer;\nimpl Packer {\n    pub fn pack(&self) {\n}\n",
                false,
            ),
            // 型の外で宣言したメンバー (Go のレシーバ)
            (
                "a.go",
                "package p\n\ntype Packer struct{}\n\nfunc (p Packer) pack() {}\n",
                false,
            ),
            (
                "a.py",
                "class Packer:\n    def pack(self):\n        pass\n\n\nclass Outer:\n    class Packer:\n        pass\n",
                false,
            ),
        ] {
            assert_eq!(
                proves_member_binding(path, source.as_bytes(), "Packer", "pack"),
                expected,
                "{path}: {source}"
            );
        }
    }
}
