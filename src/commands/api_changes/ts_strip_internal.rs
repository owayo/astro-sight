//! TypeScript の `stripInternal` (Issue #274)。
//!
//! ファイルに適用される設定 (`extends` の先を含む実効値) が `stripInternal` を有効にすると、
//! 前のコメントに `@internal` のある宣言 (export・クラスやインタフェースのメンバー・入れ子の型の
//! メンバーなど) は宣言ファイルから消える。API の差分では、宣言ファイルを使う外の利用者から見た面
//! として、そうした宣言を公開の一覧から除き、型のメンバーはシグネチャと互換の判定から除く
//! ([`published_source`])。`@internal` の付け足しは、その宣言の削除と同じ区分になる。

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;

use tree_sitter::Node;

use crate::language::LangId;
use crate::models::symbol::Symbol;

/// リポジトリの `stripInternal` の設定。TS のソースに `@internal` があるときだけ、ファイルに
/// 適用される設定を読む (設定ごとに覚える)。旧版のソースは base の版の設定で判定する (Issue #287)。
pub(crate) struct StripInternal {
    /// 今の設定 (新版のソース)。
    configs: Option<RefCell<super::ts_alias_resolution::StripInternalConfigs>>,
    /// base の版の設定を読む場所 (`dir`, 版)。[`Self::with_base`] のときだけ。
    base: Option<(String, String)>,
    /// base の版の設定 (旧版のソース)。初めて要るとき (`@internal` を含む TS のソースか、設定の
    /// 変更) に読み、`@internal` の無い差分では git を動かさない。base の tree を読めなければ None
    /// で、旧版も今の設定で判定する。
    base_configs:
        std::cell::OnceCell<Option<RefCell<super::ts_alias_resolution::StripInternalConfigs>>>,
    /// 旧新の片方の判定を決められなかったファイルの当て方 ((旧版か, パス) → 当て方)。
    /// [`Self::align`]。
    overrides: RefCell<HashMap<(bool, String), Override>>,
}

/// 判定を決められなかったファイルの、片側の当て方 ([`StripInternal::align`])。
#[derive(Clone, Debug, Default)]
struct Override {
    /// `stripInternal` を当てるか。
    strips: bool,
    /// 当てるときにも残す (消さない) `@internal` の項目の鍵 ([`items`])。
    keep: Rc<HashSet<String>>,
}

impl StripInternal {
    /// 旧版も新版も今の設定で判定する。
    pub(crate) fn new(dir: &str) -> Self {
        Self {
            configs: Some(RefCell::new(
                super::ts_alias_resolution::StripInternalConfigs::new(dir),
            )),
            base: None,
            base_configs: std::cell::OnceCell::new(),
            overrides: RefCell::default(),
        }
    }

    /// 旧版のソースを base の版 `rev` の設定で、新版を今の設定で判定する。base の tree を読めな
    /// ければ、旧版も今の設定で判定する ([`Self::has_base`] が false)。
    pub(crate) fn with_base(dir: &str, rev: &str) -> Self {
        Self {
            base: Some((dir.to_owned(), rev.to_owned())),
            ..Self::new(dir)
        }
    }

    /// 設定を読まず、`@internal` を見ない (API の差分以外の用途)。
    pub(crate) fn disabled() -> Self {
        Self {
            configs: None,
            base: None,
            base_configs: std::cell::OnceCell::new(),
            overrides: RefCell::default(),
        }
    }

    /// base の版の設定 (初めて要るときに読む)。
    fn base_configs(&self) -> Option<&RefCell<super::ts_alias_resolution::StripInternalConfigs>> {
        let (dir, rev) = self.base.as_ref()?;
        self.base_configs
            .get_or_init(|| {
                super::ts_alias_resolution::BaseTree::load(dir, rev).map(|tree| {
                    RefCell::new(super::ts_alias_resolution::StripInternalConfigs::with_fs(
                        dir,
                        super::ts_alias_resolution::ConfigFs::Base(std::rc::Rc::new(tree)),
                    ))
                })
            })
            .as_ref()
    }

    /// 旧版を base の版の設定で判定するか。
    pub(crate) fn has_base(&self) -> bool {
        self.base_configs().is_some()
    }

    /// 旧版の判定に使う設定。
    fn old_configs(&self) -> Option<&RefCell<super::ts_alias_resolution::StripInternalConfigs>> {
        self.base_configs().or(self.configs.as_ref())
    }

    /// `path` (`dir` 相対。言語 `lang`、ソース `source`) で、`@internal` の宣言を宣言ファイルから
    /// 消すか (新版)。手書きの宣言ファイル (`.d.ts` など) は出力しないので消えない。
    pub(crate) fn applies(&self, path: &str, lang: LangId, source: &[u8]) -> bool {
        self.configs.as_ref().is_some_and(|configs| {
            may_strip(path, lang, source)
                && match self.overrides.borrow().get(&(false, path.to_owned())) {
                    Some(value) => value.strips,
                    None => configs.borrow_mut().strips(path),
                }
        })
    }

    /// [`Self::applies`] の旧版 (base の版の設定で判定する)。
    pub(crate) fn applies_old(&self, path: &str, lang: LangId, source: &[u8]) -> bool {
        // `@internal` を含まないソースでは base の版の設定を読まない (git を動かさない)。
        may_strip(path, lang, source)
            && self.old_configs().is_some_and(|configs| {
                match self.overrides.borrow().get(&(true, path.to_owned())) {
                    Some(value) => value.strips,
                    None => configs.borrow_mut().strips(path),
                }
            })
    }

    /// `stripInternal` で消すかを決めるときに読む `source` (旧版なら `old`)。[`Self::align`] で残すと
    /// 決めた項目の `@internal` を、同じ長さの別の字 (`@INTERNAL`) にしたもの (位置は変えない)。
    pub(crate) fn view<'s>(
        &self,
        old: bool,
        path: &str,
        lang: LangId,
        source: &'s [u8],
    ) -> Cow<'s, [u8]> {
        let keep = match self.overrides.borrow().get(&(old, path.to_owned())) {
            Some(value) if value.strips && !value.keep.is_empty() => Rc::clone(&value.keep),
            _ => return Cow::Borrowed(source),
        };
        match crate::engine::parser::parse_source(source, lang) {
            Ok(tree) => Cow::Owned(neutralize(tree.root_node(), source, &keep)),
            Err(_) => Cow::Borrowed(source),
        }
    }

    /// 旧版 `old_path` (ソース `old_source`) と新版 `new_path` (`new_source`) の片方の判定を決め
    /// られなければ、項目ごとに当て方を決める (Issue #287・#288)。判定の違い (環境) だけによる
    /// 変化は結論せず、ソースの変化は、決められない側がどちらの判定でも壊れ得るなら出す。
    ///
    /// - 旧版を決められず新版は消す: 旧版でも消すが、新版で消えたか中身の変わった `@internal` の
    ///   項目は残す (ソースから消した宣言の削除を隠さない)。
    /// - 旧版を決められず新版は消さない: 旧版も消さない。
    /// - 新版を決められず旧版は消す: 新版も消す。
    /// - 新版を決められず旧版は消さない (旧新とも決められないときを含む): 旧版は消さず、新版は
    ///   この差分で `@internal` を付けた項目だけを消す (付け足しを削除として出し、元からの
    ///   `@internal` の項目は比べる)。
    ///
    /// 決められない側があれば true (申告する)。
    pub(crate) fn align(
        &self,
        old_path: &str,
        new_path: &str,
        old_source: Option<&[u8]>,
        new_source: Option<&[u8]>,
    ) -> bool {
        use super::ts_alias_resolution::StripState;
        let Some((old, new)) = self.states(old_path, new_path) else {
            return false;
        };
        if old != StripState::Unknown && new != StripState::Unknown {
            return false;
        }
        let parsed = |path: &str, source: Option<&[u8]>| {
            let source = source?;
            let lang =
                crate::engine::parser::detect_lang(camino::Utf8Path::new(path), source).ok()?;
            let tree = crate::engine::parser::parse_source(source, lang).ok()?;
            Some(items(tree.root_node(), source))
        };
        let old_items = parsed(old_path, old_source).unwrap_or_default();
        let new_items = parsed(new_path, new_source).unwrap_or_default();
        // `@internal` の項目のうち、`keep` の文 (項目の `owner`) 以外を消す当て方 (消すものが無ければ
        // 当てない)。
        let partial = |items: &HashMap<String, Item>, keep: HashSet<String>| Override {
            strips: items
                .values()
                .any(|item| item.internal && !keep.contains(&item.owner)),
            keep: Rc::new(keep),
        };
        let (old_override, new_override) = match (old, new) {
            (StripState::Unknown, StripState::Strips) => {
                // 新版で消えたか中身の変わった項目の文は残す。
                let keep = old_items
                    .iter()
                    .filter(|(key, item)| {
                        item.internal
                            && new_items
                                .get(*key)
                                .is_none_or(|other| other.text != item.text)
                    })
                    .map(|(_, item)| item.owner.clone())
                    .collect();
                (Some(partial(&old_items, keep)), None)
            }
            (StripState::Unknown, StripState::Keeps) => (Some(Override::default()), None),
            (StripState::Strips, StripState::Unknown) => (
                None,
                Some(Override {
                    strips: true,
                    keep: Rc::default(),
                }),
            ),
            _ => {
                // この差分で旧版からある項目に `@internal` を付けた文だけを消し、ほかの `@internal` の
                // 文は残す (新しく足した項目は付け足しではない)。
                let tagged: HashSet<&String> = new_items
                    .iter()
                    .filter(|(key, item)| {
                        item.internal && old_items.get(*key).is_some_and(|other| !other.internal)
                    })
                    .map(|(_, item)| &item.owner)
                    .collect();
                let keep = new_items
                    .values()
                    .filter(|item| item.internal && !tagged.contains(&item.owner))
                    .map(|item| item.owner.clone())
                    .collect();
                (
                    (old == StripState::Unknown).then(Override::default),
                    Some(partial(&new_items, keep)),
                )
            }
        };
        let mut overrides = self.overrides.borrow_mut();
        if let Some(old_override) = old_override {
            overrides.insert((true, old_path.to_owned()), old_override);
        }
        if let Some(new_override) = new_override {
            overrides.insert((false, new_path.to_owned()), new_override);
        }
        true
    }

    /// `path` に `stripInternal` が当たるとき、宣言ファイルの型のメンバーにした `source`
    /// ([`published_source`])。当たらない・消すものが無い・解けないときは None (新版)。
    pub(crate) fn published(&self, path: &str, lang: LangId, source: &[u8]) -> Option<Vec<u8>> {
        if !self.applies(path, lang, source) {
            return None;
        }
        let view = self.view(false, path, lang, source);
        let tree = crate::engine::parser::parse_source(&view, lang).ok()?;
        published_source(tree.root_node(), &view)
    }

    /// [`Self::published`] の旧版。
    pub(crate) fn published_old(&self, path: &str, lang: LangId, source: &[u8]) -> Option<Vec<u8>> {
        if !self.applies_old(path, lang, source) {
            return None;
        }
        let view = self.view(true, path, lang, source);
        let tree = crate::engine::parser::parse_source(&view, lang).ok()?;
        published_source(tree.root_node(), &view)
    }

    /// 旧版 `old_path` と新版 `new_path` に当たる設定の判定 (`@internal` の有無を問わない)。
    /// base の版の設定を読めないときは None (旧新を比べ直さない)。
    pub(crate) fn states(
        &self,
        old_path: &str,
        new_path: &str,
    ) -> Option<(
        super::ts_alias_resolution::StripState,
        super::ts_alias_resolution::StripState,
    )> {
        let base = self.base_configs()?;
        let configs = self.configs.as_ref()?;
        let old = base.borrow_mut().strips_state(old_path);
        let new = configs.borrow_mut().strips_state(new_path);
        Some((old, new))
    }
}

/// `@internal` の宣言を宣言ファイルから消し得るソースか (TS の出力するソースで、`@internal` を
/// 含む)。手書きの宣言ファイル (`.d.ts` など) は出力しない。
pub(crate) fn may_strip(path: &str, lang: LangId, source: &[u8]) -> bool {
    matches!(lang, LangId::Typescript | LangId::Tsx)
        && !super::ts_module_paths::is_declaration_path(path)
        && source
            .windows(b"@internal".len())
            .any(|window| window == b"@internal")
}

/// `stripInternal` で消し得る項目 1 つ ([`items`])。
#[derive(Debug)]
struct Item {
    /// 前のコメントに `@internal` があり、宣言ファイルに写す位置にある。
    internal: bool,
    /// 本文のトークン ([`fingerprint`])。
    text: String,
    /// `@internal` を読む文の鍵 ([`neutralize`] で残す単位)。変数の文は、宣言ごとの項目が文を
    /// 共有する。
    owner: String,
}

/// 項目を辿るときのノード。
#[derive(Clone, Copy)]
enum ItemNode<'tree> {
    /// `@internal` を読む文やメンバー。
    Anchor(Node<'tree>),
    /// 変数の文 `statement` の宣言 1 つ。
    Binding {
        statement: Node<'tree>,
        declarator: Node<'tree>,
    },
}

/// 宣言を並べる本体の直下の文 (export 文を含む) か、型・クラス・enum のメンバーかタプルの要素か。
fn is_item(node: Node<'_>) -> bool {
    node.is_named()
        && node.kind() != "comment"
        && (node
            .parent()
            .is_some_and(|parent| is_declaration_body(parent.kind()))
            || is_strippable_type_element(node))
}

/// 本文の構造とトークン (コメントを除く。名前のあるノードの種類と境界を含め、`return` の後の
/// 改行で式が切れるような構造の違いも区別する。文字列・テンプレート・正規表現は中身の空白も保つ)。
fn fingerprint(node: Node<'_>, source: &[u8]) -> String {
    enum Step<'tree> {
        Enter(Node<'tree>),
        Exit,
    }
    let mut out = String::new();
    let mut stack = vec![Step::Enter(node)];
    while let Some(step) = stack.pop() {
        let Step::Enter(node) = step else {
            out.push('\u{3}');
            continue;
        };
        match node.kind() {
            "comment" => continue,
            "string" | "template_string" | "regex" => {}
            _ if node.child_count() > 0 => {
                if node.is_named() {
                    out.push_str(node.kind());
                    out.push('\u{2}');
                    stack.push(Step::Exit);
                }
                let mut cursor = node.walk();
                let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
                stack.extend(children.into_iter().rev().map(Step::Enter));
                continue;
            }
            _ => {}
        }
        out.push_str(node.utf8_text(source).unwrap_or_default());
        out.push('\u{1}');
    }
    out
}

/// 変数の文 (export 文を含む) なら、その宣言のノード。
fn variable_declaration(node: Node<'_>) -> Option<Node<'_>> {
    let declaration = declaration_of(node);
    matches!(
        declaration.kind(),
        "lexical_declaration" | "variable_declaration"
    )
    .then_some(declaration)
}

/// 項目の名前 (宣言の名前。名前の無いものは種類と本文)。
fn item_name(node: Node<'_>, source: &[u8]) -> String {
    let declaration = declaration_of(node);
    if let Some(name) = declaration.child_by_field_name("name") {
        return fingerprint(name, source);
    }
    match declaration.kind() {
        "property_identifier" | "string" => fingerprint(declaration, source),
        kind => format!("{kind}:{}", fingerprint(declaration, source)),
    }
}

/// 前のコメントに `@internal` があり、宣言ファイルに写す位置にあるか。
fn is_internal_item(node: Node<'_>, source: &[u8]) -> bool {
    has_internal_comment(node, source) && is_emitted_position(node)
}

/// ソースの項目 (鍵 → 項目)。鍵は、囲む項目の名前と自身の名前をつないだもの (同じ鍵は現れた順の
/// 番号を付ける。オーバーロードなど)。変数の文は宣言ごとに項目にする (`const a = 1, b = 2` の
/// 宣言を足し引きしても、ほかの宣言は同じ鍵で対応する)。
fn items(root: Node<'_>, source: &[u8]) -> HashMap<String, Item> {
    let mut items = HashMap::new();
    visit_items(root, source, &mut |node, key, owner| {
        let item = match node {
            ItemNode::Anchor(anchor) if variable_declaration(anchor).is_some() => return,
            ItemNode::Anchor(anchor) => Item {
                internal: is_internal_item(anchor, source),
                text: fingerprint(anchor, source),
                owner: owner.to_owned(),
            },
            ItemNode::Binding {
                statement,
                declarator,
            } => {
                // `const`・`let`・`var` の違いも宣言ファイルに出る。
                let keyword = variable_declaration(statement)
                    .and_then(|declaration| declaration.child(0))
                    .and_then(|keyword| keyword.utf8_text(source).ok())
                    .unwrap_or_default();
                Item {
                    internal: is_internal_item(statement, source),
                    text: format!("{keyword}\u{1}{}", fingerprint(declarator, source)),
                    owner: owner.to_owned(),
                }
            }
        };
        items.insert(key.to_owned(), item);
    });
    items
}

/// 項目を文書の順に辿る (ノード, [`items`] の鍵, `@internal` を読む文の鍵)。
fn visit_items(root: Node<'_>, source: &[u8], visit: &mut impl FnMut(ItemNode<'_>, &str, &str)) {
    fn key(prefix: &str, name: &str, seen: &mut HashMap<String, usize>) -> String {
        let base = format!("{prefix}\u{1f}{name}");
        let count = seen.entry(base.clone()).or_insert(0);
        *count += 1;
        if *count == 1 {
            base
        } else {
            format!("{base}#{count}")
        }
    }
    fn walk(
        node: Node<'_>,
        prefix: &str,
        source: &[u8],
        seen: &mut HashMap<String, usize>,
        visit: &mut impl FnMut(ItemNode<'_>, &str, &str),
    ) {
        let mut cursor = node.walk();
        let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
        for child in children {
            if !is_item(child) {
                walk(child, prefix, source, seen, visit);
                continue;
            }
            let Some(declaration) = variable_declaration(child) else {
                let item = key(prefix, &item_name(child, source), seen);
                visit(ItemNode::Anchor(child), &item, &item);
                walk(child, &item, source, seen, visit);
                continue;
            };
            let mut cursor = declaration.walk();
            let declarators: Vec<Node<'_>> = declaration
                .named_children(&mut cursor)
                .filter(|declarator| declarator.kind() == "variable_declarator")
                .collect();
            let names: Vec<String> = declarators
                .iter()
                .filter_map(|declarator| declarator.child_by_field_name("name"))
                .map(|name| fingerprint(name, source))
                .collect();
            let owner = key(prefix, &format!("var:{}", names.join(",")), seen);
            visit(ItemNode::Anchor(child), &owner, &owner);
            for declarator in declarators {
                let name = declarator
                    .child_by_field_name("name")
                    .map(|name| fingerprint(name, source))
                    .unwrap_or_default();
                let binding = key(prefix, &name, seen);
                visit(
                    ItemNode::Binding {
                        statement: child,
                        declarator,
                    },
                    &binding,
                    &owner,
                );
                walk(declarator, &binding, source, seen, visit);
            }
        }
    }
    walk(root, "", source, &mut HashMap::new(), visit);
}

/// 鍵が `keep` の文の前のコメントの `@internal` を `@INTERNAL` にした `source` (位置は変えない)。
fn neutralize(root: Node<'_>, source: &[u8], keep: &HashSet<String>) -> Vec<u8> {
    let mut ranges = Vec::new();
    visit_items(root, source, &mut |node, key, _| {
        if let ItemNode::Anchor(anchor) = node
            && keep.contains(key)
        {
            ranges.extend(
                leading_comments(anchor, source)
                    .iter()
                    .map(|comment| comment.byte_range()),
            );
        }
    });
    let mut view = source.to_vec();
    for range in ranges {
        let Some(comment) = view.get_mut(range) else {
            continue;
        };
        let mut index = 0;
        while index + b"@internal".len() <= comment.len() {
            if &comment[index..index + b"@internal".len()] == b"@internal" {
                comment[index..index + b"@internal".len()].copy_from_slice(b"@INTERNAL");
                index += b"@internal".len();
            } else {
                index += 1;
            }
        }
    }
    view
}

/// TypeScript の改行 (`\n`・`\r`・U+2028・U+2029) があるか。
fn has_line_break(bytes: &[u8]) -> bool {
    bytes.iter().any(|byte| matches!(byte, b'\n' | b'\r'))
        || bytes
            .windows(3)
            .any(|window| matches!(window, [0xE2, 0x80, 0xA8 | 0xA9]))
}

/// 宣言 `node` の leading comment (ソースの順)。TypeScript の `getLeadingCommentRanges` と同じく、
/// 前のトークンの後の、コメントの外の最初の改行より後に始まるコメント (ファイルの先頭の宣言は
/// 先頭からの全て) で、前のトークンと同じ行のコメントと、その行から続く複数行のコメントは含まない。
fn leading_comments<'tree>(node: Node<'tree>, source: &[u8]) -> Vec<Node<'tree>> {
    let mut comments = Vec::new();
    let mut current = node.prev_sibling();
    let preceding = loop {
        match current {
            Some(sibling) if sibling.kind() == "comment" => {
                comments.push(sibling);
                current = sibling.prev_sibling();
            }
            other => break other,
        }
    };
    comments.reverse();
    let mut collecting = preceding.is_none();
    let mut cursor = preceding.map_or(0, |token| token.end_byte());
    let mut leading = Vec::new();
    for comment in comments {
        if !collecting {
            collecting = source
                .get(cursor..comment.start_byte())
                .is_none_or(has_line_break);
        }
        if collecting {
            leading.push(comment);
        }
        cursor = comment.end_byte();
    }
    leading
}

/// 宣言 `node` の leading comment に `@internal` があるか。TypeScript の `isInternalDeclaration`
/// と同じく、JSDoc に限らずコメントの字面で見る。
pub(crate) fn has_internal_comment(node: Node<'_>, source: &[u8]) -> bool {
    leading_comments(node, source).iter().any(|comment| {
        comment
            .utf8_text(source)
            .is_ok_and(|text| text.contains("@internal"))
    })
}

/// 宣言を並べる本体 (モジュール・namespace・クラス・インタフェース・型・enum)。
fn is_declaration_body(kind: &str) -> bool {
    matches!(
        kind,
        "program"
            | "statement_block"
            | "class_body"
            | "interface_body"
            | "object_type"
            | "enum_body"
    )
}

/// 型のメンバーの種類 (object 型と interface の本体の中)。
fn is_type_member(kind: &str) -> bool {
    matches!(
        kind,
        "property_signature"
            | "method_signature"
            | "call_signature"
            | "construct_signature"
            | "index_signature"
    )
}

/// `statement` (export 文も) の中の宣言。
fn declaration_of(statement: Node<'_>) -> Node<'_> {
    if statement.kind() == "export_statement" {
        statement
            .child_by_field_name("declaration")
            .unwrap_or(statement)
    } else {
        statement
    }
}

fn named(node: Node<'_>, source: &[u8], name: &str) -> bool {
    node.child_by_field_name("name")
        .and_then(|name| name.utf8_text(source).ok())
        == Some(name)
}

/// `anchor` (本体 `body` の直下) が実装の関数・メソッドなら、同じ名前のオーバーロードの宣言
/// (宣言ファイルに出るのはこちら)。実装でなければ None。
fn overloads<'tree>(
    anchor: Node<'tree>,
    body: Node<'tree>,
    source: &[u8],
    name: &str,
) -> Option<Vec<Node<'tree>>> {
    let implementation = declaration_of(anchor);
    let signature_kind = match implementation.kind() {
        "function_declaration" => "function_signature",
        "method_definition" => "method_signature",
        _ => return None,
    };
    let mut cursor = body.walk();
    Some(
        body.named_children(&mut cursor)
            .filter(|statement| {
                let declaration = declaration_of(*statement);
                declaration.kind() == signature_kind && named(declaration, source, name)
            })
            .collect(),
    )
}

/// モジュールの `export { name }` 文 (from 句の無いもの)。
fn export_clause_statements<'tree>(
    program: Node<'tree>,
    source: &[u8],
    name: &str,
) -> Vec<Node<'tree>> {
    let mut cursor = program.walk();
    program
        .named_children(&mut cursor)
        .filter(|statement| {
            statement.kind() == "export_statement"
                && statement.child_by_field_name("source").is_none()
                && statement
                    .named_children(&mut statement.walk())
                    .filter(|clause| clause.kind() == "export_clause")
                    .any(|clause| {
                        clause
                            .named_children(&mut clause.walk())
                            .filter(|specifier| specifier.kind() == "export_specifier")
                            .any(|specifier| named(specifier, source, name))
                    })
        })
        .collect()
}

/// 本体の直下の宣言 `anchor` が、`@internal` で宣言ファイルから消えるか。実装の関数・メソッドは
/// オーバーロードの宣言が出るので、オーバーロードが全て `@internal` のとき消える。`export` の
/// 無い宣言を `export { name }` で公開しているなら、その文が全て `@internal` のときも公開から
/// 外れる (オーバーロードのある関数も)。
fn anchor_is_internal(anchor: Node<'_>, body: Node<'_>, source: &[u8], name: &str) -> bool {
    let declared_internal = match overloads(anchor, body, source, name) {
        Some(overloads) if !overloads.is_empty() => overloads
            .iter()
            .all(|overload| has_internal_comment(*overload, source)),
        _ => has_internal_comment(anchor, source),
    };
    if declared_internal {
        return true;
    }
    if body.kind() == "program" && anchor.kind() != "export_statement" {
        let statements = export_clause_statements(body, source, name);
        return !statements.is_empty()
            && statements
                .iter()
                .all(|statement| has_internal_comment(*statement, source));
    }
    false
}

/// シンボル `sym` の宣言が、それ自体か囲む宣言の `@internal` で宣言ファイルから消えるか。本体の
/// 直下まで上ったノード (export 文を含む) ごとに、その前のコメントを見る。式の中の本体 (クラス式の
/// メンバー) は推論した型として出るので、そのメンバーの `@internal` では消えない。
pub(crate) fn is_internal_symbol(sym: &Symbol, root: Node<'_>, source: &[u8]) -> bool {
    let start = tree_sitter::Point {
        row: sym.range.start.line,
        column: sym.range.start.column,
    };
    let end = tree_sitter::Point {
        row: sym.range.end.line,
        column: sym.range.end.column,
    };
    let Some(mut node) = root.descendant_for_point_range(start, end) else {
        return false;
    };
    let bare = sym.name.rsplit('.').next().unwrap_or(&sym.name);
    let mut innermost = true;
    while let Some(parent) = node.parent() {
        if is_declaration_body(parent.kind()) {
            let internal = is_emitted_position(parent)
                && if innermost {
                    anchor_is_internal(node, parent, source, bare)
                } else {
                    has_internal_comment(node, source)
                };
            if internal {
                return true;
            }
            innermost = false;
            if parent.kind() == "program" {
                return false;
            }
        }
        node = parent;
    }
    false
}

/// モジュールの `export { .. }` 文 (from 句の有無を問わない) が公開する名前のうち、`@internal` の
/// 文だけが公開するもの (宣言ファイルから消える)。
pub(crate) fn internal_only_export_names(
    root: Node<'_>,
    source: &[u8],
) -> std::collections::HashSet<String> {
    let mut internal = std::collections::HashSet::new();
    let mut public = std::collections::HashSet::new();
    let mut cursor = root.walk();
    for statement in root
        .named_children(&mut cursor)
        .filter(|statement| statement.kind() == "export_statement")
    {
        let target = if has_internal_comment(statement, source) {
            &mut internal
        } else {
            &mut public
        };
        let mut clauses = statement.walk();
        for clause in statement
            .named_children(&mut clauses)
            .filter(|clause| clause.kind() == "export_clause")
        {
            let mut specifiers = clause.walk();
            for specifier in clause
                .named_children(&mut specifiers)
                .filter(|specifier| specifier.kind() == "export_specifier")
            {
                let name = specifier
                    .child_by_field_name("alias")
                    .or_else(|| specifier.child_by_field_name("name"))
                    .and_then(|name| name.utf8_text(source).ok());
                if let Some(name) = name {
                    target.insert(name.to_owned());
                }
            }
        }
    }
    internal.retain(|name| !public.contains(name));
    internal
}

/// 宣言ファイルから消え得る型の要素か: object 型と interface のメンバー (mapped type の節は
/// 宣言ではないので消えない) と、タプルの要素 (ラベルの有無、`T?`・`...T[]` を問わない)。
fn is_strippable_type_element(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        "object_type" | "interface_body" => {
            is_type_member(node.kind())
                && !(node.kind() == "index_signature"
                    && node
                        .children(&mut node.walk())
                        .any(|child| child.kind() == "mapped_type_clause"))
        }
        "tuple_type" => node.is_named() && node.kind() != "comment",
        // クラスのメンバー (フィールド・メソッド・アクセサ・宣言・index signature) と enum の
        // メンバーも、宣言ファイルから消える (Issue #286)。
        "class_body" => matches!(
            node.kind(),
            "public_field_definition"
                | "method_definition"
                | "method_signature"
                | "abstract_method_signature"
                | "index_signature"
        ),
        "enum_body" => matches!(
            node.kind(),
            "property_identifier" | "string" | "number" | "enum_assignment"
        ),
        _ => false,
    }
}

/// 宣言ファイルを作るとき `stripInternal` で消える、本体の無い関数の宣言の文か (オーバーロードと
/// ambient の宣言。export 文も)。実装のある宣言とクラスなどは、シンボルごとに除く
/// ([`is_internal_symbol`])。オーバーロードの一部だけが `@internal` なら、残りのオーバーロードが
/// 宣言ファイルに出る (Issue #286)。
fn is_stripped_signature_statement(node: Node<'_>, source: &[u8]) -> bool {
    node.parent()
        .is_some_and(|parent| matches!(parent.kind(), "program" | "statement_block"))
        && declaration_of(node).kind() == "function_signature"
        && has_internal_comment(node, source)
        && is_emitted_position(node)
}

/// 宣言ファイルを作るとき `stripInternal` で消えるメンバーか ([`published_source`] が空白にする
/// もの)。
pub(crate) fn is_stripped_member(node: Node<'_>, source: &[u8]) -> bool {
    is_strippable_type_element(node)
        && has_internal_comment(node, source)
        && is_emitted_position(node)
}

/// 式の種類。式の中の型 (アロー関数の引数・`as` の型など) は、宣言ファイルでは推論した型として
/// 出るので、`@internal` のメンバーも消えない。
fn is_expression(kind: &str) -> bool {
    matches!(
        kind,
        "arrow_function"
            | "function_expression"
            | "generator_function"
            | "class"
            | "as_expression"
            | "satisfies_expression"
            | "type_assertion"
            | "call_expression"
            | "new_expression"
            | "object"
            | "array"
            | "pair"
            | "spread_element"
            | "arguments"
            | "parenthesized_expression"
            | "sequence_expression"
            | "assignment_expression"
            | "augmented_assignment_expression"
            | "binary_expression"
            | "unary_expression"
            | "update_expression"
            | "ternary_expression"
            | "await_expression"
            | "yield_expression"
            | "instantiation_expression"
            | "member_expression"
            | "subscript_expression"
            | "non_null_expression"
            | "template_string"
            | "decorator"
    )
}

/// `node` (`export_statement` の直下) が、名前の無い `export default` のクラス・関数の宣言か。
/// 文法では式のノードになるが、TypeScript では宣言として宣言ファイルにそのまま写す。
fn is_default_export_declaration(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "class" | "function_expression" | "generator_function"
    ) && node.parent().is_some_and(|parent| {
        parent.kind() == "export_statement"
            && parent
                .children(&mut parent.walk())
                .any(|child| child.kind() == "default")
    })
}

/// `node` が、宣言ファイルを作るとき TypeScript が宣言や型をそのまま写す位置にあるか。式の中
/// (推論した型として出る) と関数の本体の中 (出ない) は写さない。型の中の `typeof` の式
/// (`typeof f<{ .. }>` の型引数など) は型として写す。
fn is_emitted_position(node: Node<'_>) -> bool {
    let mut child = node;
    // 下に式があったか (上に `typeof` の型があれば、その式は型の一部)。
    let mut in_expression = false;
    while let Some(parent) = child.parent() {
        if parent.kind() == "type_query" {
            in_expression = false;
        } else if is_expression(parent.kind()) && !is_default_export_declaration(parent) {
            in_expression = true;
        }
        if matches!(
            parent.kind(),
            "function_declaration"
                | "generator_function_declaration"
                | "method_definition"
                | "class_static_block"
        ) && parent
            .child_by_field_name("body")
            .is_some_and(|body| body.id() == child.id())
        {
            return false;
        }
        child = parent;
    }
    !in_expression
}

/// 宣言ファイルの型のメンバー: `source` の、宣言ファイルから消える `@internal` の型の要素
/// (interface と object 型のメンバーとラベル付きのタプルの要素。関数の引数や戻り値・交差型の中
/// など、入れ子の型も) を空白にしたもの。宣言ファイルに型をそのまま写す位置のものだけで、式の中の
/// 型 (推論した型として出る) は残す。要素と一緒に消える leading comment・区切り・trailing comment
/// も空白にする。改行は残すので、位置 (バイト・行・列) は元のソースと変わらない。消すものが無ければ
/// None。
pub(crate) fn published_source(root: Node<'_>, source: &[u8]) -> Option<Vec<u8>> {
    let mut ranges = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if is_stripped_member(node, source) || is_stripped_signature_statement(node, source) {
            ranges.push(member_extent(node, source));
            continue;
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    if ranges.is_empty() {
        return None;
    }
    let mut published = source.to_vec();
    for range in ranges {
        for byte in published.get_mut(range)? {
            if !matches!(*byte, b'\n' | b'\r') {
                *byte = b' ';
            }
        }
    }
    Some(published)
}

/// 宣言ファイルから消える型の要素 `member` の範囲。TypeScript ではメンバーの区切り (`;`・`,`)
/// はメンバーの一部なので、間のコメントを越えて区切りまでを含める。leading comment と、続く
/// 同じ行のコメント (trailing comment) もメンバーと一緒に消える。
fn member_extent(member: Node<'_>, source: &[u8]) -> Range<usize> {
    let start = leading_comments(member, source)
        .first()
        .map_or(member.start_byte(), Node::start_byte);
    let mut end = member.end_byte();
    let mut next = member.next_sibling();
    while let Some(comment) = next.filter(|node| node.kind() == "comment") {
        next = comment.next_sibling();
    }
    match next.filter(|node| matches!(node.kind(), ";" | ",")) {
        Some(separator) => {
            end = separator.end_byte();
            next = separator.next_sibling();
        }
        None => next = member.next_sibling(),
    }
    while let Some(comment) = next.filter(|node| node.kind() == "comment") {
        if source
            .get(end..comment.start_byte())
            .is_none_or(has_line_break)
        {
            break;
        }
        end = comment.end_byte();
        next = comment.next_sibling();
    }
    start..end
}
