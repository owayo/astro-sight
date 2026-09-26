//! 宣言ヘッダ (関数 / メソッドの呼び出し契約) を AST から取り出し、変更前後で比べる。
//!
//! `signature.rs` の行ベースの検出は「関数名を含む `-` 行と `+` 行」同士しか比べない。
//! そのため次の 2 つを取りこぼし、呼び出し側を壊す変更が cross-file 検索から外れていた。
//!
//! 1. 複数行にまたがる引数リストの継続行だけの変更 (`+    b: u32,`)。関数名の行が
//!    変わらないので「本体だけの変更」と判定され、フィルタ 3 で落ちる。
//! 2. 同じファイル内での移動 + シグネチャ変更。移動先の hunk は純追加なので
//!    `change_type` が `"added"` になり、フィルタ 6 で落ちる。
//!
//! どちらも「変更前の同じ宣言」と比べれば判定できる。変更前のソースは diff を new 側へ
//! 逆適用して復元し (`diff::reconstruct_old_source`)、同じ識別 (名前 / 種別 / container)
//! の宣言のヘッダをトークン列で比較する。行ベースの検出が結論を出せる場合 (1 行の
//! ヘッダで変更前後の行が揃っている) はそちらを優先し、ここは補完に徹する。

use std::collections::HashSet;

use tree_sitter::Node;

use crate::engine::{diff, parser, symbols};
use crate::language::LangId;
use crate::models::impact::{AffectedSymbol, HunkInfo, SignatureChange};
use crate::models::symbol::Symbol;

use super::signature::find_signature_in_lines;

/// 宣言ノードから本体ノードを探す方法。
#[derive(Clone, Copy)]
enum BodyLocator {
    /// `body` フィールド。無ければ本体を持たない宣言 (trait の必須メソッドや抽象メソッド) で、
    /// 宣言全体がヘッダになる。
    BodyField,
    /// 直下の子のうち指定した種別のノード (Kotlin の `function_body` はフィールドを持たない)。
    ChildKind(&'static str),
}

/// 関数 / メソッドのシンボルが指す宣言ノードの種別と、その本体の探し方。
///
/// シンボル抽出 (`symbols::symbol_query`) が関数 / メソッドとして捕捉する宣言ノードを
/// 列挙する。ここに無い形のノードは判定せず、行ベースの検出結果をそのまま使う
/// (本体の位置を誤るとボディの変更をヘッダの変更と取り違えるため、推測で広げない)。
fn function_declaration_shapes(lang_id: LangId) -> &'static [(&'static str, BodyLocator)] {
    use BodyLocator::{BodyField, ChildKind};
    match lang_id {
        LangId::Rust => &[
            ("function_item", BodyField),
            ("function_signature_item", BodyField),
        ],
        LangId::C | LangId::Cpp => &[("function_definition", BodyField)],
        LangId::Python => &[("function_definition", BodyField)],
        LangId::Javascript | LangId::Typescript | LangId::Tsx => &[
            ("function_declaration", BodyField),
            ("generator_function_declaration", BodyField),
            ("method_definition", BodyField),
        ],
        LangId::Go => &[
            ("function_declaration", BodyField),
            ("method_declaration", BodyField),
        ],
        LangId::Php => &[
            ("function_definition", BodyField),
            ("method_declaration", BodyField),
        ],
        LangId::Java => &[("method_declaration", BodyField)],
        LangId::Kotlin => &[("function_declaration", ChildKind("function_body"))],
        LangId::Swift => &[
            ("function_declaration", BodyField),
            ("protocol_function_declaration", BodyField),
        ],
        LangId::CSharp => &[("method_declaration", BodyField)],
        LangId::Bash => &[("function_definition", BodyField)],
        LangId::Ruby => &[("method", BodyField), ("singleton_method", BodyField)],
        // `test_declaration` も関数として捕捉されるが、本体が `body` フィールドを持たず
        // 呼び出し契約も無いので対象外。
        LangId::Zig => &[("function_declaration", BodyField)],
        // lexer-only 言語は AST を持たない。
        LangId::Xojo => &[],
    }
}

/// 変数に関数を束縛する宣言 (`const f = (a) => a`) で、値として現れる関数ノードの種別。
/// 値ノードの `body` フィールドが本体になる。
fn function_value_kinds(lang_id: LangId) -> &'static [&'static str] {
    match lang_id {
        LangId::Javascript | LangId::Typescript | LangId::Tsx => &[
            "arrow_function",
            "function_expression",
            "generator_function",
        ],
        LangId::Rust
        | LangId::C
        | LangId::Cpp
        | LangId::Python
        | LangId::Go
        | LangId::Php
        | LangId::Java
        | LangId::Kotlin
        | LangId::Swift
        | LangId::CSharp
        | LangId::Bash
        | LangId::Ruby
        | LangId::Zig
        | LangId::Xojo => &[],
    }
}

/// 宣言ノードの本体を返す。外側の `None` は判定対象外の形、`Some(None)` は本体を持たない
/// 宣言 (宣言全体がヘッダ)。
fn declaration_body<'a>(decl: Node<'a>, lang_id: LangId) -> Option<Option<Node<'a>>> {
    if decl.kind() == "variable_declarator" {
        let value = decl.child_by_field_name("value")?;
        if !function_value_kinds(lang_id).contains(&value.kind()) {
            return None;
        }
        return Some(value.child_by_field_name("body"));
    }
    let &(_, locator) = function_declaration_shapes(lang_id)
        .iter()
        .find(|(kind, _)| *kind == decl.kind())?;
    Some(match locator {
        BodyLocator::BodyField => decl.child_by_field_name("body"),
        BodyLocator::ChildKind(kind) => {
            let mut cursor = decl.walk();
            decl.children(&mut cursor)
                .find(|child| child.kind() == kind)
        }
    })
}

/// 関数 / メソッド宣言の「呼び出し側から見える宣言ヘッダ」。
///
/// 範囲は名前ノードのある行の行頭から、本体の直前にある要素の終端まで。名前より前の行
/// だけに置かれた注釈 (`@Override` / `#[inline]` 等) は含めない — 行ベースの検出と同じく
/// 名前を含む行を起点にし、注釈の付け外しをシグネチャ変更として扱わないため。
pub(super) struct DeclarationHeader {
    /// 名前ノードのある行 (0-indexed)。
    first_line: usize,
    /// 本体直前の要素の終端行 (0-indexed)。本体が無ければ宣言の終端行。
    last_line: usize,
    /// 比較用のトークン列。コメントと Rust の束縛側 `mut` を除き、閉じ括弧直前の末尾カンマを
    /// 落とす (書式の違いだけでシグネチャ変更にしない)。
    tokens: Vec<String>,
    /// 出力用のヘッダ文字列 (コメントを除き空白を畳んだもの)。
    display: String,
}

impl DeclarationHeader {
    fn is_multi_line(&self) -> bool {
        self.first_line != self.last_line
    }

    /// ヘッダの行範囲に追加行か削除位置があるか。
    ///
    /// 削除位置 (gap) は「削除の直後に来る new 側行」なので、`gap == first_line` は名前の行
    /// より前 (注釈行など) の削除を指し、ヘッダ内の変更ではない。
    fn has_changes(&self, facts: &diff::ChangedLineFacts) -> bool {
        (self.first_line..=self.last_line).any(|line| facts.added_lines.contains(&line))
            || facts
                .deletion_gaps
                .iter()
                .any(|&gap| gap > self.first_line && gap <= self.last_line)
    }

    fn same_contract(&self, other: &Self) -> bool {
        self.tokens == other.tokens
    }
}

/// シンボルの宣言ヘッダを取り出す。判定対象外の宣言や名前ノードが見つからない場合は `None`。
pub(super) fn declaration_header(
    root: Node<'_>,
    source: &[u8],
    symbol: &Symbol,
    lang_id: LangId,
) -> Option<DeclarationHeader> {
    let start = tree_sitter::Point {
        row: symbol.range.start.line,
        column: symbol.range.start.column,
    };
    let end = tree_sitter::Point {
        row: symbol.range.end.line,
        column: symbol.range.end.column,
    };
    let decl = root.descendant_for_point_range(start, end)?;
    let body = declaration_body(decl, lang_id)?;
    let (header_end, last_line) = match body {
        Some(body) => {
            let before_body = body.prev_sibling()?;
            (before_body.end_byte(), before_body.end_position().row)
        }
        None => (decl.end_byte(), decl.end_position().row),
    };
    let body_id = body.map(|b| b.id());

    if lang_id == LangId::Python {
        let display = crate::engine::python_signature::normalize_function_header(decl, source)?;
        return Some(DeclarationHeader {
            first_line: decl.child_by_field_name("name")?.start_position().row,
            last_line,
            tokens: vec![display.clone()],
            display,
        });
    }

    // 名前ノード: 本体の外にある、シンボル名と同じテキストを持つ最初の名前付き葉。
    let mut name_node = None;
    let mut leaves: Vec<Node<'_>> = Vec::new();
    // (開始, 終了) のうち表示・比較から外す範囲 (コメント、Rust の束縛側 `mut`)。
    let mut omitted: Vec<(usize, usize)> = Vec::new();
    let mut cursor = decl.walk();
    let mut descend = true;
    loop {
        let node = cursor.node();
        let skip_subtree = Some(node.id()) == body_id || is_omitted_token(node, lang_id);
        if is_omitted_token(node, lang_id) {
            omitted.push((node.start_byte(), node.end_byte()));
        }
        if !skip_subtree && node.child_count() == 0 {
            if name_node.is_none()
                && node.is_named()
                && node.utf8_text(source).ok() == Some(symbol.name.as_str())
            {
                name_node = Some(node);
            }
            leaves.push(node);
        }
        if descend && !skip_subtree && cursor.goto_first_child() {
            continue;
        }
        if cursor.goto_next_sibling() {
            descend = true;
            continue;
        }
        if !cursor.goto_parent() || cursor.node().id() == decl.id() {
            break;
        }
        descend = false;
    }
    let name_node = name_node?;
    let first_line = name_node.start_position().row;
    let line_start = source[..name_node.start_byte()]
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |pos| pos + 1);
    if header_end < line_start {
        return None;
    }

    let mut tokens: Vec<String> = leaves
        .iter()
        .filter(|leaf| leaf.start_byte() >= line_start && leaf.end_byte() <= header_end)
        .filter_map(|leaf| leaf.utf8_text(source).ok().map(str::to_string))
        .collect();
    drop_trailing_commas(&mut tokens);

    Some(DeclarationHeader {
        first_line,
        last_line,
        tokens,
        display: header_display(source, line_start, header_end, &omitted),
    })
}

/// 比較・表示から外すトークンか。コメント類 (`is_extra`) と、Rust の引数の束縛側 `mut`
/// (`fn f(mut x: T)` の `mut` は本体側の束縛指定で呼び出し契約ではない。行ベースの
/// 検出も `rust_signature::normalize_rust_signature_text` で同じものを除いている)。
fn is_omitted_token(node: Node<'_>, lang_id: LangId) -> bool {
    node.is_extra()
        || (lang_id == LangId::Rust
            && node.kind() == "mutable_specifier"
            && node.parent().is_some_and(|p| p.kind() == "parameter"))
}

/// 閉じ括弧の直前にある末尾カンマを落とす (`(a, b,)` と `(a, b)` を同じ契約として扱う)。
fn drop_trailing_commas(tokens: &mut Vec<String>) {
    let mut out: Vec<String> = Vec::with_capacity(tokens.len());
    for token in tokens.drain(..) {
        if matches!(token.as_str(), ")" | "]" | ">" | "}")
            && out.last().is_some_and(|prev| prev == ",")
        {
            out.pop();
        }
        out.push(token);
    }
    *tokens = out;
}

/// ヘッダの表示用文字列。除外範囲を空白に置き換え、空白を畳み、括弧の内側の空白と
/// 閉じ括弧直前の末尾カンマを落とす (`fn f(\n    a: u32,\n)` → `fn f(a: u32)`)。
fn header_display(source: &[u8], start: usize, end: usize, omitted: &[(usize, usize)]) -> String {
    let mut bytes: Vec<u8> = source[start..end].to_vec();
    for &(s, e) in omitted {
        if s >= start && e <= end {
            bytes[s - start..e - start].fill(b' ');
        }
    }
    let text = String::from_utf8_lossy(&bytes);
    let collapsed: Vec<char> = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .collect();
    let mut out = String::with_capacity(collapsed.len());
    for (i, &c) in collapsed.iter().enumerate() {
        if c == ' '
            && (out.ends_with(['(', '['])
                || collapsed
                    .get(i + 1)
                    .is_some_and(|next| matches!(next, ')' | ']' | ',')))
        {
            continue;
        }
        if matches!(c, ')' | ']') && out.ends_with(',') {
            out.pop();
        }
        out.push(c);
    }
    out
}

/// Pass 1 の 1 ファイル分の入力。
pub(super) struct DeclarationChangeInput<'a> {
    /// 対象ファイルの diff 区間。
    pub(super) file_diff: &'a str,
    pub(super) file_path: &'a str,
    pub(super) syms: &'a [Symbol],
    pub(super) hunks: &'a [HunkInfo],
    pub(super) root: Node<'a>,
    /// new 側のソース (`--staged` では index の内容)。
    pub(super) source: &'a [u8],
    pub(super) lang_id: LangId,
    pub(super) facts: &'a diff::ChangedLineFacts,
}

/// 復元した変更前ソースの解析結果。
struct OldSide {
    source: Vec<u8>,
    tree: tree_sitter::Tree,
    symbols: Vec<Symbol>,
    removed_lines: HashSet<usize>,
}

/// 変更前ソースは必要になったときだけ 1 度復元する (多くのファイルでは使わない)。
enum LazyOldSide {
    Pending,
    Unavailable,
    Ready(OldSide),
}

impl LazyOldSide {
    fn get(&mut self, input: &DeclarationChangeInput<'_>) -> Option<&OldSide> {
        if matches!(self, LazyOldSide::Pending) {
            *self = build_old_side(input).map_or(LazyOldSide::Unavailable, LazyOldSide::Ready);
        }
        match self {
            LazyOldSide::Ready(old) => Some(old),
            LazyOldSide::Pending | LazyOldSide::Unavailable => None,
        }
    }
}

fn build_old_side(input: &DeclarationChangeInput<'_>) -> Option<OldSide> {
    let reconstructed =
        diff::reconstruct_old_source(input.file_diff, input.file_path, input.source)?;
    let tree = parser::parse_source(&reconstructed.source, input.lang_id).ok()?;
    let symbols =
        symbols::extract_symbols(tree.root_node(), &reconstructed.source, input.lang_id).ok()?;
    Some(OldSide {
        source: reconstructed.source,
        tree,
        symbols,
        removed_lines: reconstructed.removed_lines,
    })
}

/// 名前 / 種別 / container が同じ (= 同じ宣言の変更前後とみなせる) か。
fn same_identity(a: &Symbol, b: &Symbol) -> bool {
    a.name == b.name && a.kind == b.kind && a.container == b.container
}

impl OldSide {
    /// `symbol` と同じ識別を持つ変更前の宣言ヘッダ。
    fn counterpart_headers(&self, symbol: &Symbol, lang_id: LangId) -> Vec<DeclarationHeader> {
        self.symbols
            .iter()
            .filter(|old| same_identity(old, symbol))
            .filter_map(|old| declaration_header(self.tree.root_node(), &self.source, old, lang_id))
            .collect()
    }

    /// `symbol` の移動元になりうる変更前の宣言ヘッダ。
    ///
    /// 同じ識別を持ち、行が**すべて**削除された (= 元の位置から消えた) 宣言に限る。一部の行
    /// だけが削除された宣言はその場で書き換えられた既存の宣言で、`symbol` は新しく足された
    /// 別の宣言 (オーバーロード等) である。さらに、`symbol` 以外の変更後の宣言
    /// (`new_twins`) と呼び出し契約が一致する宣言は、そちらに引き継がれたものとして除く
    /// (その場で全行を書き換えた既存の宣言 + 新しいオーバーロードの追加を移動と誤認しない)。
    fn moved_away_headers(
        &self,
        symbol: &Symbol,
        lang_id: LangId,
        new_twins: &[DeclarationHeader],
    ) -> Vec<DeclarationHeader> {
        self.symbols
            .iter()
            .filter(|old| same_identity(old, symbol))
            .filter(|old| {
                (old.range.start.line..=old.range.end.line)
                    .all(|line| self.removed_lines.contains(&line))
            })
            .filter_map(|old| declaration_header(self.tree.root_node(), &self.source, old, lang_id))
            .filter(|old| !new_twins.iter().any(|twin| twin.same_contract(old)))
            .collect()
    }
}

/// 行ベースのシグネチャ検出を AST の宣言ヘッダ比較で補う。
///
/// - `"added"` と判定された関数のうち、同じファイルの変更前に同じ識別の宣言があり、その
///   行がすべて削除されている (= 移動した、`OldSide::moved_away_headers`) ものは、ヘッダが
///   変わっていれば `"modified"` に戻し、シグネチャ変更を記録する。ヘッダが同じ (純粋な
///   移動や本体だけの変更) なら従来どおり `"added"` のまま (呼び出し契約は変わっていない)。
///   既存の宣言をその場で書き換えつつ同名のオーバーロードを足した変更は移動ではない。
/// - `"modified"` の関数でヘッダの行範囲に変更があるのに行ベースの検出が結論を出せなかった
///   もの (複数行のヘッダ、関数名の行が無変更) は、変更前の同じ宣言とヘッダを比べる。
///
/// 1 行のヘッダで変更前後の行が揃っている場合は行ベースの結果を優先する (既存の検出と
/// 正規化 (Rust の束縛側 `mut` など) を変えないため)。
pub(super) fn reconcile_declaration_changes(
    input: &DeclarationChangeInput<'_>,
    affected: &mut [AffectedSymbol],
    sig_changes: &mut Vec<SignatureChange>,
) {
    let file_has_removed_lines = !input.facts.deletion_gaps.is_empty();
    let mut old_side = LazyOldSide::Pending;
    // 行ベースの検出が変更前後のシグネチャ行を揃えられたかを判定するための変更行。
    let mut changed_line_texts: Option<(Vec<String>, Vec<String>)> = None;

    for symbol_change in affected.iter_mut() {
        // variable は JS/TS の `const f = (a) => ...` のように関数を束縛するものだけが
        // 判定対象になる (`declaration_body` が他の変数を弾く)。
        if !matches!(
            symbol_change.kind.as_str(),
            "function" | "method" | "variable"
        ) {
            continue;
        }
        let is_added = symbol_change.change_type == "added";
        if is_added && !file_has_removed_lines {
            continue;
        }
        let has_text_sig_change = sig_changes.iter().any(|sc| sc.name == symbol_change.name);
        // Python は単一の宣言を新旧とも確認できる場合、共通のトークン比較を正本にする。
        // 行比較の空白誤検出を打ち消す一方、文字列内部の空白変更の見逃しも補う。
        // 同名の別メソッド・overload や構文エラーがあれば従来の検出を維持する。
        if !is_added && input.lang_id == LangId::Python && !input.root.has_error() {
            let mut matching = input.syms.iter().filter(|s| s.name == symbol_change.name);
            if let Some(symbol) = matching.next()
                && matching.next().is_none()
                && let Some(old) = old_side.get(input)
                && !old.tree.root_node().has_error()
                && old.symbols.iter().filter(|s| s.name == symbol.name).count() == 1
                && let [old_header] = old.counterpart_headers(symbol, input.lang_id).as_slice()
                && let Some(new_header) =
                    declaration_header(input.root, input.source, symbol, input.lang_id)
            {
                if old_header.same_contract(&new_header) {
                    sig_changes.retain(|sc| sc.name != symbol_change.name);
                } else if !has_text_sig_change {
                    sig_changes.push(SignatureChange {
                        name: symbol_change.name.clone(),
                        old_signature: old_header.display.clone(),
                        new_signature: new_header.display,
                    });
                }
                continue;
            }
        }
        if !is_added && has_text_sig_change {
            continue;
        }
        // affected は名前しか持たないため、同名のシンボル (別 container の同名メソッド・
        // オーバーロード等) を取り違えないよう、`find_affected_symbols` と同じ判定で同じ
        // 変更種別になるシンボルだけを候補にする (その場で書き換えた既存の宣言の "modified" に、
        // 新しく足したオーバーロードを当てない)。
        let change_type = symbol_change.change_type.as_str();
        let candidates = input.syms.iter().filter(|s| {
            s.name == symbol_change.name
                && super::classify_symbol_change(s, input.hunks, Some(input.facts))
                    == Some(change_type)
        });
        let mut found: Option<SignatureChange> = None;
        for symbol in candidates {
            let Some(new_header) =
                declaration_header(input.root, input.source, symbol, input.lang_id)
            else {
                continue;
            };
            if !is_added {
                if !new_header.has_changes(input.facts) {
                    continue;
                }
                // 1 行のヘッダで行ベースの検出が変更前後の行を揃えられたなら、その結論
                // (変更なし) を採る。複数行のヘッダは関数名の行しか比べていないので補う。
                if !new_header.is_multi_line()
                    && matches!(symbol_change.kind.as_str(), "function" | "method")
                    && text_path_compared(&mut changed_line_texts, input, &symbol_change.name)
                {
                    continue;
                }
            }
            let Some(old) = old_side.get(input) else {
                break;
            };
            // "added" は移動元 (元の位置から消えた同じ宣言) とだけ比べる。
            let old_headers = if is_added {
                let new_twins: Vec<DeclarationHeader> = input
                    .syms
                    .iter()
                    .filter(|other| !std::ptr::eq(*other, symbol) && same_identity(other, symbol))
                    .filter_map(|other| {
                        declaration_header(input.root, input.source, other, input.lang_id)
                    })
                    .collect();
                old.moved_away_headers(symbol, input.lang_id, &new_twins)
            } else {
                old.counterpart_headers(symbol, input.lang_id)
            };
            if old_headers.is_empty() || old_headers.iter().any(|h| h.same_contract(&new_header)) {
                continue;
            }
            found = Some(SignatureChange {
                name: symbol_change.name.clone(),
                old_signature: old_headers[0].display.clone(),
                new_signature: new_header.display,
            });
            break;
        }
        let Some(change) = found else {
            continue;
        };
        if is_added {
            symbol_change.change_type = "modified".to_string();
        }
        if !has_text_sig_change {
            sig_changes.push(change);
        }
    }
}

/// 行ベースの検出 (`detect_signature_changes`) が `name` の変更前後のシグネチャ行を
/// 両方見つけられたか。見つけられたなら、1 行のヘッダについては行ベースの結論を採る。
fn text_path_compared(
    changed_line_texts: &mut Option<(Vec<String>, Vec<String>)>,
    input: &DeclarationChangeInput<'_>,
    name: &str,
) -> bool {
    let (removed, added) = changed_line_texts.get_or_insert_with(|| {
        super::signature::collect_changed_line_texts(input.file_diff, input.file_path)
    });
    find_signature_in_lines(removed, name, input.lang_id).is_some()
        && find_signature_in_lines(added, name, input.lang_id).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト用: `source` を解析し、名前が `name` のシンボルの宣言ヘッダを返す。
    fn header_of(lang_id: LangId, source: &str, name: &str) -> DeclarationHeader {
        let tree = parser::parse_source(source.as_bytes(), lang_id).expect("parse");
        let syms =
            symbols::extract_symbols(tree.root_node(), source.as_bytes(), lang_id).expect("syms");
        let symbol = syms
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("{lang_id}: symbol {name} not found in {syms:?}"));
        declaration_header(tree.root_node(), source.as_bytes(), symbol, lang_id)
            .unwrap_or_else(|| panic!("{lang_id}: header of {name} not extracted"))
    }

    /// テスト用: `old` → `new` の unified diff (context 3 行) を LCS で作る。
    /// 同じファイル内の移動が「削除 hunk + 追加 hunk」に分かれる、git と同じ形の diff にする。
    fn unified_diff(path: &str, old: &str, new: &str) -> String {
        let a: Vec<&str> = old.lines().collect();
        let b: Vec<&str> = new.lines().collect();
        let (n, m) = (a.len(), b.len());
        let mut lcs = vec![vec![0usize; m + 1]; n + 1];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                lcs[i][j] = if a[i] == b[j] {
                    lcs[i + 1][j + 1] + 1
                } else {
                    lcs[i + 1][j].max(lcs[i][j + 1])
                };
            }
        }
        // (種別, 次の old 行, 次の new 行)。' ' = 共通、'-' = 削除、'+' = 追加。
        let mut ops: Vec<(char, usize, usize)> = Vec::new();
        let (mut i, mut j) = (0, 0);
        while i < n || j < m {
            if i < n && j < m && a[i] == b[j] {
                ops.push((' ', i, j));
                i += 1;
                j += 1;
            } else if i < n && (j == m || lcs[i + 1][j] >= lcs[i][j + 1]) {
                ops.push(('-', i, j));
                i += 1;
            } else {
                ops.push(('+', i, j));
                j += 1;
            }
        }
        // 間の共通行が 6 行以下の変更は同じ hunk にまとめ、前後 3 行を context にする。
        let mut groups: Vec<(usize, usize)> = Vec::new();
        for (k, op) in ops.iter().enumerate() {
            if op.0 == ' ' {
                continue;
            }
            match groups.last_mut() {
                Some((_, last)) if k - *last <= 7 => *last = k,
                _ => groups.push((k, k)),
            }
        }
        let mut out = format!("--- a/{path}\n+++ b/{path}\n");
        for (first, last) in groups {
            let hunk = &ops[first.saturating_sub(3)..=(last + 3).min(ops.len() - 1)];
            let old_count = hunk.iter().filter(|op| op.0 != '+').count();
            let new_count = hunk.iter().filter(|op| op.0 != '-').count();
            let old_start = hunk[0].1 + usize::from(old_count > 0);
            let new_start = hunk[0].2 + usize::from(new_count > 0);
            out.push_str(&format!(
                "@@ -{old_start},{old_count} +{new_start},{new_count} @@\n"
            ));
            for &(kind, i, j) in hunk {
                let text = if kind == '+' { b[j] } else { a[i] };
                out.push(kind);
                out.push_str(text);
                out.push('\n');
            }
        }
        out
    }

    /// テスト用: Pass 1 と同じ順 (affected 判定 → 行ベースのシグネチャ検出 → 本補完) で実行する。
    /// `reconcile` が false なら本補完を呼ばない (修正前の挙動)。
    fn run_pass1(
        lang_id: LangId,
        path: &str,
        old: &str,
        new: &str,
        reconcile: bool,
    ) -> (Vec<AffectedSymbol>, Vec<SignatureChange>) {
        let diff_text = unified_diff(path, old, new);
        let hunks = diff::parse_unified_diff(&diff_text)
            .into_iter()
            .find(|f| f.new_path == path)
            .expect("diff file")
            .hunks;
        let tree = parser::parse_source(new.as_bytes(), lang_id).expect("parse");
        let root = tree.root_node();
        let syms = symbols::extract_symbols(root, new.as_bytes(), lang_id).expect("syms");
        let facts = diff::extract_changed_line_facts(&diff_text, path);
        let mut affected = super::super::find_affected_symbols(&syms, &hunks, Some(&facts));
        let mut sig_changes =
            super::super::signature::detect_signature_changes(&diff_text, path, &affected, lang_id);
        if reconcile {
            reconcile_declaration_changes(
                &DeclarationChangeInput {
                    file_diff: &diff_text,
                    file_path: path,
                    syms: &syms,
                    hunks: &hunks,
                    root,
                    source: new.as_bytes(),
                    lang_id,
                    facts: &facts,
                },
                &mut affected,
                &mut sig_changes,
            );
        }
        (affected, sig_changes)
    }

    fn change_type_of<'a>(affected: &'a [AffectedSymbol], name: &str) -> Option<&'a str> {
        affected
            .iter()
            .find(|a| a.name == name)
            .map(|a| a.change_type.as_str())
    }

    /// 本体の探し方の表に書いたノード名・フィールド名が、その文法に実在すること。
    ///
    /// 名前を誤っても 1 つもマッチしなくなるだけでエラーにならず、「宣言ヘッダの比較が
    /// 黙って行われない = 複数行の引数変更を見逃す」という無害に見える壊れ方をする
    /// (`complexity.rs` の `branch_node_kinds_exist_in_every_grammar` と同じ理由)。
    #[test]
    fn declaration_tables_exist_in_every_grammar() {
        for &lang_id in LangId::ALL_TREE_SITTER {
            let language = lang_id.ts_language();
            let shapes = function_declaration_shapes(lang_id);
            assert!(!shapes.is_empty(), "{lang_id}: 関数宣言の形が 1 つも無い");
            let mut kinds: Vec<&str> = function_value_kinds(lang_id).to_vec();
            for &(kind, locator) in shapes {
                kinds.push(kind);
                match locator {
                    BodyLocator::BodyField => assert!(
                        language.field_id_for_name("body").is_some(),
                        "{lang_id}: `body` フィールドが文法に無い ({kind})"
                    ),
                    BodyLocator::ChildKind(child) => kinds.push(child),
                }
            }
            if !function_value_kinds(lang_id).is_empty() {
                kinds.push("variable_declarator");
                assert!(language.field_id_for_name("value").is_some());
            }
            for kind in kinds {
                assert!(
                    tree_sitter::Query::new(&language, &format!("({kind}) @probe")).is_ok(),
                    "{lang_id}: 実在しないノード名 \"{kind}\""
                );
            }
        }
    }

    /// 全言語で「継続行への引数追加はヘッダの変更」「本体だけの変更はヘッダの変更ではない」を
    /// 区別できること。`(言語, シンボル名, 変更前, 引数を追加, 本体だけ変更)`。
    #[test]
    fn multi_line_parameter_change_is_a_header_change_in_every_language() {
        let cases: &[(LangId, &str, &str, Option<&str>, &str)] = &[
            (
                LangId::Rust,
                "helper",
                "pub fn helper(\n    a: u32,\n) -> u32 {\n    a\n}\n",
                Some("pub fn helper(\n    a: u32,\n    b: u32,\n) -> u32 {\n    a\n}\n"),
                "pub fn helper(\n    a: u32,\n) -> u32 {\n    a + 1\n}\n",
            ),
            (
                LangId::Rust,
                "req",
                "pub trait T {\n    fn req(\n        &self,\n    ) -> u32;\n}\n",
                Some(
                    "pub trait T {\n    fn req(\n        &self,\n        a: u32,\n    ) -> u32;\n}\n",
                ),
                "pub trait T {\n    fn req(\n        &self,\n    ) -> u32;\n}\n",
            ),
            (
                LangId::Python,
                "helper",
                "def helper(\n    a,\n):\n    return a\n",
                Some("def helper(\n    a,\n    b,\n):\n    return a\n"),
                "def helper(\n    a,\n):\n    return a + 1\n",
            ),
            (
                LangId::Javascript,
                "helper",
                "export function helper(\n  a,\n) {\n  return a;\n}\n",
                Some("export function helper(\n  a,\n  b,\n) {\n  return a;\n}\n"),
                "export function helper(\n  a,\n) {\n  return a + 1;\n}\n",
            ),
            (
                LangId::Javascript,
                "arrow",
                "export const arrow = (\n  a,\n) => a;\n",
                Some("export const arrow = (\n  a,\n  b,\n) => a;\n"),
                "export const arrow = (\n  a,\n) => a + 1;\n",
            ),
            (
                LangId::Typescript,
                "meth",
                "class K {\n  meth(\n    a: number,\n  ): number {\n    return a;\n  }\n}\n",
                Some(
                    "class K {\n  meth(\n    a: number,\n    b: number,\n  ): number {\n    return a;\n  }\n}\n",
                ),
                "class K {\n  meth(\n    a: number,\n  ): number {\n    return a + 1;\n  }\n}\n",
            ),
            (
                LangId::Tsx,
                "helper",
                "export function helper(\n  a: number,\n): number {\n  return a;\n}\n",
                Some(
                    "export function helper(\n  a: number,\n  b: number,\n): number {\n  return a;\n}\n",
                ),
                "export function helper(\n  a: number,\n): number {\n  return a + 1;\n}\n",
            ),
            (
                LangId::Go,
                "Helper",
                "package main\n\nfunc Helper(\n\ta int,\n) int {\n\treturn a\n}\n",
                Some("package main\n\nfunc Helper(\n\ta int,\n\tb int,\n) int {\n\treturn a\n}\n"),
                "package main\n\nfunc Helper(\n\ta int,\n) int {\n\treturn a + 1\n}\n",
            ),
            (
                LangId::Java,
                "helper",
                "class A {\n    public int helper(\n        int a\n    ) {\n        return a;\n    }\n}\n",
                Some(
                    "class A {\n    public int helper(\n        int a,\n        int b\n    ) {\n        return a;\n    }\n}\n",
                ),
                "class A {\n    public int helper(\n        int a\n    ) {\n        return a + 1;\n    }\n}\n",
            ),
            (
                LangId::Kotlin,
                "helper",
                "fun helper(\n    a: Int,\n): Int {\n    return a\n}\n",
                Some("fun helper(\n    a: Int,\n    b: Int,\n): Int {\n    return a\n}\n"),
                "fun helper(\n    a: Int,\n): Int {\n    return a + 1\n}\n",
            ),
            (
                LangId::Swift,
                "helper",
                "func helper(\n    a: Int\n) -> Int {\n    return a\n}\n",
                Some("func helper(\n    a: Int,\n    b: Int\n) -> Int {\n    return a\n}\n"),
                "func helper(\n    a: Int\n) -> Int {\n    return a + 1\n}\n",
            ),
            (
                LangId::CSharp,
                "Helper",
                "class A {\n    public int Helper(\n        int a\n    )\n    {\n        return a;\n    }\n}\n",
                Some(
                    "class A {\n    public int Helper(\n        int a,\n        int b\n    )\n    {\n        return a;\n    }\n}\n",
                ),
                "class A {\n    public int Helper(\n        int a\n    )\n    {\n        return a + 1;\n    }\n}\n",
            ),
            (
                LangId::Php,
                "helper",
                "<?php\nfunction helper(\n    int $a\n): int {\n    return $a;\n}\n",
                Some(
                    "<?php\nfunction helper(\n    int $a,\n    int $b\n): int {\n    return $a;\n}\n",
                ),
                "<?php\nfunction helper(\n    int $a\n): int {\n    return $a + 1;\n}\n",
            ),
            (
                LangId::Ruby,
                "helper",
                "def helper(\n  a\n)\n  a\nend\n",
                Some("def helper(\n  a,\n  b\n)\n  a\nend\n"),
                "def helper(\n  a\n)\n  a + 1\nend\n",
            ),
            (
                LangId::C,
                "helper",
                "int helper(\n    int a\n) {\n    return a;\n}\n",
                Some("int helper(\n    int a,\n    int b\n) {\n    return a;\n}\n"),
                "int helper(\n    int a\n) {\n    return a + 1;\n}\n",
            ),
            (
                LangId::Cpp,
                "helper",
                "int helper(\n    int a\n) {\n    return a;\n}\n",
                Some("int helper(\n    int a,\n    int b\n) {\n    return a;\n}\n"),
                "int helper(\n    int a\n) {\n    return a + 1;\n}\n",
            ),
            (
                LangId::Zig,
                "helper",
                "pub fn helper(\n    a: u32,\n) u32 {\n    return a;\n}\n",
                Some("pub fn helper(\n    a: u32,\n    b: u32,\n) u32 {\n    return a;\n}\n"),
                "pub fn helper(\n    a: u32,\n) u32 {\n    return a + 1;\n}\n",
            ),
            // bash の関数は引数リストを持たない。本体だけの変更を区別できることだけ確かめる。
            (
                LangId::Bash,
                "helper",
                "helper() {\n  echo a\n}\n",
                None,
                "helper() {\n  echo b\n}\n",
            ),
        ];
        let mut covered: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for &(lang_id, name, old, param_added, body_changed) in cases {
            covered.insert(lang_id.to_string());
            let old_header = header_of(lang_id, old, name);
            let body_header = header_of(lang_id, body_changed, name);
            assert!(
                old_header.same_contract(&body_header),
                "{lang_id} {name}: 本体だけの変更をヘッダの変更と扱ってはいけない: {:?} vs {:?}",
                old_header.tokens,
                body_header.tokens
            );
            if let Some(param_added) = param_added {
                let param_header = header_of(lang_id, param_added, name);
                assert!(
                    old_header.is_multi_line(),
                    "{lang_id} {name}: 複数行のヘッダとして取り出すこと"
                );
                assert!(
                    !old_header.same_contract(&param_header),
                    "{lang_id} {name}: 継続行への引数追加はヘッダの変更: {:?}",
                    param_header.tokens
                );
            }
        }
        for &lang_id in LangId::ALL_TREE_SITTER {
            assert!(
                covered.contains(&lang_id.to_string()),
                "{lang_id} の検証ケースが無い"
            );
        }
    }

    /// 呼び出し契約が変わらない書き換えはヘッダの変更として扱わない (誤検出の抑制)。
    /// 対照として、同じ位置の型の変更は検出し続ける。
    #[test]
    fn contract_preserving_rewrites_are_not_header_changes() {
        let base = header_of(
            LangId::Rust,
            "pub fn helper(a: u32, b: &mut u32) -> u32 {\n    a\n}\n",
            "helper",
        );
        for (label, source) in [
            (
                "1 行 → 複数行 + 末尾カンマ",
                "pub fn helper(\n    a: u32,\n    b: &mut u32,\n) -> u32 {\n    a\n}\n",
            ),
            (
                "引数リスト内のコメント",
                "pub fn helper(\n    a: u32, // first\n    b: &mut u32,\n) -> u32 {\n    a\n}\n",
            ),
            (
                "束縛側の mut (呼び出し契約ではない)",
                "pub fn helper(\n    mut a: u32,\n    b: &mut u32,\n) -> u32 {\n    a\n}\n",
            ),
        ] {
            assert!(
                base.same_contract(&header_of(LangId::Rust, source, "helper")),
                "{label} はヘッダの変更ではない"
            );
        }
        // 対照: 型側の `&mut` → `&` は呼び出し契約の変更。
        assert!(!base.same_contract(&header_of(
            LangId::Rust,
            "pub fn helper(\n    a: u32,\n    b: &u32,\n) -> u32 {\n    a\n}\n",
            "helper",
        )));
        // 名前より前の行だけに置いた注釈はヘッダに含めない (行ベースの検出と同じ起点)。
        let java = "class A {\n    public int helper(int a) {\n        return a;\n    }\n}\n";
        let annotated = "class A {\n    @Deprecated\n    public int helper(int a) {\n        return a;\n    }\n}\n";
        assert!(
            header_of(LangId::Java, java, "helper").same_contract(&header_of(
                LangId::Java,
                annotated,
                "helper"
            ))
        );
    }

    /// 複数行の引数リストの継続行だけを変えた変更を、シグネチャ変更として記録する。
    /// 行ベースの検出は関数名を含む行同士しか比べないため、補完が無いと「本体だけの変更」扱いで
    /// cross-file 検索から落ちていた。
    #[test]
    fn multi_line_parameter_addition_is_recorded_as_signature_change() {
        let old = "pub fn helper(\n    a: u32,\n) -> u32 {\n    a\n}\n";
        let new = "pub fn helper(\n    a: u32,\n    b: u32,\n) -> u32 {\n    a\n}\n";
        let (_, before) = run_pass1(LangId::Rust, "src/util.rs", old, new, false);
        assert!(
            before.is_empty(),
            "前提: 行ベースの検出だけでは拾えない: {before:?}"
        );

        let (affected, sig_changes) = run_pass1(LangId::Rust, "src/util.rs", old, new, true);
        assert_eq!(change_type_of(&affected, "helper"), Some("modified"));
        assert_eq!(sig_changes.len(), 1, "{sig_changes:?}");
        assert_eq!(sig_changes[0].old_signature, "pub fn helper(a: u32) -> u32");
        assert_eq!(
            sig_changes[0].new_signature,
            "pub fn helper(a: u32, b: u32) -> u32"
        );

        // 対照: 同じ関数の本体だけの変更はシグネチャ変更にしない。
        let body_only = "pub fn helper(\n    a: u32,\n) -> u32 {\n    a + 1\n}\n";
        let (_, sig_changes) = run_pass1(LangId::Rust, "src/util.rs", old, body_only, true);
        assert!(sig_changes.is_empty(), "{sig_changes:?}");

        // JS/TS の関数を束縛した変数 (`const f = (...) => ...`) も同じ。
        let old_ts = "export const arrow = (\n  a: number,\n): number => a;\n";
        let new_ts = "export const arrow = (\n  a: number,\n  b: number,\n): number => a;\n";
        let (_, sig_changes) = run_pass1(LangId::Typescript, "src/a.ts", old_ts, new_ts, true);
        assert_eq!(sig_changes.len(), 1, "{sig_changes:?}");
        let body_ts = "export const arrow = (\n  a: number,\n): number => a + 1;\n";
        let (_, sig_changes) = run_pass1(LangId::Typescript, "src/a.ts", old_ts, body_ts, true);
        assert!(sig_changes.is_empty(), "{sig_changes:?}");
    }

    /// 同じファイル内で移動しつつシグネチャを変えた関数は "added" ではなく "modified"。
    /// 移動先の hunk は純追加なので "added" と判定され、フィルタ 6 で cross-file 検索から
    /// 外れて呼び出し側を見逃していた。
    #[test]
    fn moved_function_with_signature_change_is_modified_not_added() {
        let old = "pub fn helper(a: u32) -> u32 {\n    a + 1\n}\n\npub fn other() -> u32 {\n    1\n}\n\npub fn third() -> u32 {\n    3\n}\n\npub fn fourth() -> u32 {\n    4\n}\n";
        let moved_changed = "pub fn other() -> u32 {\n    1\n}\n\npub fn third() -> u32 {\n    3\n}\n\npub fn fourth() -> u32 {\n    4\n}\n\npub fn helper(a: u32, b: u32) -> u32 {\n    a + b\n}\n";
        let (before, _) = run_pass1(LangId::Rust, "src/util.rs", old, moved_changed, false);
        assert_eq!(
            change_type_of(&before, "helper"),
            Some("added"),
            "前提: 移動先の hunk は純追加なので行ベースでは added になる"
        );
        let (affected, sig_changes) =
            run_pass1(LangId::Rust, "src/util.rs", old, moved_changed, true);
        assert_eq!(change_type_of(&affected, "helper"), Some("modified"));
        assert!(
            sig_changes.iter().any(|sc| sc.name == "helper"),
            "{sig_changes:?}"
        );

        // 複数行のヘッダで移動 + 引数追加 (関数名の行が変わらないので行ベースでは拾えない)。
        let old_ml = "pub fn helper(\n    a: u32,\n) -> u32 {\n    a\n}\n\npub fn other() -> u32 {\n    1\n}\n\npub fn third() -> u32 {\n    3\n}\n\npub fn fourth() -> u32 {\n    4\n}\n";
        let moved_ml = "pub fn other() -> u32 {\n    1\n}\n\npub fn third() -> u32 {\n    3\n}\n\npub fn fourth() -> u32 {\n    4\n}\n\npub fn helper(\n    a: u32,\n    b: u32,\n) -> u32 {\n    a\n}\n";
        let (affected, sig_changes) =
            run_pass1(LangId::Rust, "src/util.rs", old_ml, moved_ml, true);
        assert_eq!(change_type_of(&affected, "helper"), Some("modified"));
        assert_eq!(sig_changes.len(), 1, "{sig_changes:?}");
        assert_eq!(
            sig_changes[0].new_signature,
            "pub fn helper(a: u32, b: u32) -> u32"
        );

        // 対照: 呼び出し契約を変えない移動 (純粋な移動 / 本体だけの変更) は従来どおり added。
        for (label, new) in [
            (
                "純粋な移動",
                "pub fn other() -> u32 {\n    1\n}\n\npub fn third() -> u32 {\n    3\n}\n\npub fn fourth() -> u32 {\n    4\n}\n\npub fn helper(a: u32) -> u32 {\n    a + 1\n}\n",
            ),
            (
                "移動 + 本体だけの変更",
                "pub fn other() -> u32 {\n    1\n}\n\npub fn third() -> u32 {\n    3\n}\n\npub fn fourth() -> u32 {\n    4\n}\n\npub fn helper(a: u32) -> u32 {\n    a + 2\n}\n",
            ),
        ] {
            let (affected, sig_changes) = run_pass1(LangId::Rust, "src/util.rs", old, new, true);
            assert_eq!(
                change_type_of(&affected, "helper"),
                Some("added"),
                "{label}: {affected:?}"
            );
            assert!(sig_changes.is_empty(), "{label}: {sig_changes:?}");
        }

        // 対照: 同じ名前でも別の型のメソッド (container 違い) の追加は移動ではない。
        // 変更同士を 7 行以上離し、B::run の追加を独立した純追加 hunk にする
        // (近接すると削除を含む混在 hunk になり、行ベースで既に modified になる)。
        let old_impl = "pub struct A;\npub struct B;\n\nimpl A {\n    pub fn run(&self, x: u32) -> u32 {\n        x\n    }\n}\n\npub fn f1() -> u32 {\n    1\n}\n\npub fn f2() -> u32 {\n    2\n}\n";
        let new_impl = "pub struct A;\npub struct B;\n\nimpl A {\n    pub fn run(&self, x: u64) -> u64 {\n        x\n    }\n}\n\npub fn f1() -> u32 {\n    1\n}\n\npub fn f2() -> u32 {\n    2\n}\n\nimpl B {\n    pub fn run(&self) -> u32 {\n        0\n    }\n}\n";
        let (affected, _) = run_pass1(LangId::Rust, "src/util.rs", old_impl, new_impl, true);
        let mut run_change_types: Vec<&str> = affected
            .iter()
            .filter(|a| a.name == "run")
            .map(|a| a.change_type.as_str())
            .collect();
        run_change_types.sort_unstable();
        assert_eq!(
            run_change_types,
            vec!["added", "modified"],
            "A::run はその場の変更、B::run は新規追加のまま: {affected:?}"
        );
    }

    /// 同名のオーバーロードを足した変更を、既存の宣言の「移動 + シグネチャ変更」と取り違えない。
    /// (1) 既存の `f(int a)` の本体をその場で書き換えた場合、行が 1 行でも削除された宣言を
    /// 移動元とみなすと、足した `f(int a, int b)` の移動元と誤認する。(2) 既存の `f(int a)` を
    /// 契約を変えずに移動した場合、移動元の `f(int a)` は移動後の自分自身に引き継がれており、
    /// 足した方の移動元ではない。どちらも呼び出し側を壊さない変更なのにブロックしていた。
    #[test]
    fn adding_overload_is_not_mistaken_for_move_of_existing_declaration() {
        let others = "\n    public int g() {\n        return 1;\n    }\n\n    public int h() {\n        return 2;\n    }\n\n    public int k() {\n        return 3;\n    }\n";
        let overload = "\n    public int f(int a, int b) {\n        return a + b;\n    }\n";
        let multi_line_f = |body: &str| {
            format!(
                "    public int f(int a) {{\n        int x = a + {body};\n        return x;\n    }}\n"
            )
        };
        let cases = [
            (
                "既存の宣言の本体だけを書き換え",
                format!("class Calc {{\n{}{others}}}\n", multi_line_f("1")),
                format!("class Calc {{\n{}{others}{overload}}}\n", multi_line_f("2")),
                vec!["added", "modified"],
            ),
            (
                "既存の宣言を移動 (契約は同じ) + オーバーロードを追加",
                format!("class Calc {{\n{}{others}}}\n", multi_line_f("1")),
                format!("class Calc {{{others}\n{}{overload}}}\n", multi_line_f("1")),
                vec!["added", "added"],
            ),
        ];
        for (label, old, new, expected) in &cases {
            let (affected, sig_changes) = run_pass1(LangId::Java, "Calc.java", old, new, true);
            let mut change_types: Vec<&str> = affected
                .iter()
                .filter(|a| a.name == "f")
                .map(|a| a.change_type.as_str())
                .collect();
            change_types.sort_unstable();
            assert_eq!(
                &change_types, expected,
                "{label}: 足したオーバーロードは新規追加のまま: {affected:?}"
            );
            assert!(sig_changes.is_empty(), "{label}: {sig_changes:?}");
        }

        // 対照: 元の宣言が消え、離れた位置に契約の違う同名の宣言が現れたら移動 + 変更。
        let old = format!("class Calc {{\n{}{others}}}\n", multi_line_f("1"));
        let new = format!("class Calc {{{others}{overload}}}\n");
        let (affected, sig_changes) = run_pass1(LangId::Java, "Calc.java", &old, &new, true);
        assert_eq!(
            change_type_of(&affected, "f"),
            Some("modified"),
            "{affected:?}"
        );
        assert_eq!(sig_changes.len(), 1, "{sig_changes:?}");
    }
}
