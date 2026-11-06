//! impact streaming Pass で使うフィルタ群。
//!
//! ここにまとめる理由:
//!   - import 文判定などは `&str` を受け取って `bool` を返すだけの純粋関数で、state を持たない。
//!   - Pass 2 / Pass 3 の両方から呼ばれる共通ユーティリティ。
//!   - テストを同居させておくと import 文の言語別バリエーションを追加しやすい。
//!
//! Pass 1 の cross-file 収録判定 (`CrossFileFilterContext`) も、判定材料が per-file 定数
//! だけで副作用を持たない同種のフィルタなのでここに置く。
use std::collections::{HashMap, HashSet};

use crate::engine::symbols;
use crate::language::LangId;
use crate::models::impact::{AffectedSymbol, SignatureChange};
use crate::models::symbol::Symbol;

use super::declaration;
use super::signature::is_symbol_in_changed_lines;
use super::test_context::is_in_test_context;

/// Pass 1 の cross-file 収録判定に必要な per-file 定数をまとめた借用コンテキスト。
///
/// `should_include_for_cross_file` は affected シンボル 1 件ごとに呼ばれるが、判定材料の
/// うち変化するのは `sym` だけで、残りはすべて「その diff ファイル」に固定された値。
/// ファイルごとに 1 度だけ組み立てて affected ループから使い回す。
pub(super) struct CrossFileFilterContext<'a> {
    pub(super) sig_changes: &'a [SignatureChange],
    pub(super) rust_kind_changes: &'a HashSet<String>,
    pub(super) rust_alias_changes: &'a HashSet<String>,
    /// 初期値だけを変えた非公開の Rust の const / static (`declaration::rust_value_only_constants`)。
    pub(super) rust_value_only_constants: &'a HashMap<String, declaration::RustValueOnlyConstant>,
    pub(super) rust_private_method_owners: &'a HashMap<String, String>,
    /// 対象ファイルの diff 区間 (`diff::FileSections`。シンボル名が変更行に出現するかの
    /// 照合に使う)。diff 全体を渡しても結果は同じだが、シンボルごとに走査するため遅い。
    pub(super) diff_input: &'a str,
    /// diff の新側パス。
    pub(super) file_path: &'a str,
    pub(super) root: tree_sitter::Node<'a>,
    pub(super) source: &'a [u8],
    pub(super) lang_id: LangId,
    /// 新側の実変更行 (`+` 行、0-indexed)。
    pub(super) changed_new_lines: &'a HashSet<usize>,
    /// 新側の削除位置 (`diff::ChangedLineFacts::deletion_gaps`)。
    pub(super) deletion_gaps: &'a HashSet<usize>,
}

impl CrossFileFilterContext<'_> {
    /// affected シンボルを cross-file 参照検索に含めるべきか判定する。
    ///
    /// 5段階のフィルタを適用する：
    /// 1. impl ブロックの型名をスキップ（API に影響しない）
    /// 2. テストコンテキスト内のシンボルをスキップ
    /// 3. ボディのみの変更（シグネチャ変更なし）の関数/メソッドをスキップ
    /// 4. エクスポートされていないシンボルをスキップ
    /// 5. 変更された diff 行にシンボル名が出現しない場合スキップ
    ///
    /// `original` は `sym` を作った元のシンボル。判定はすべてこのシンボルで行う
    /// (名前で引き直すと、同名のローカル const・入れ子の関数・非公開メソッドを取り違える)。
    pub(super) fn should_include_for_cross_file(
        &self,
        sym: &AffectedSymbol,
        original: &Symbol,
    ) -> bool {
        // 1. impl ブロックの型名とモジュール宣言をスキップ
        // モジュール宣言（例: `pub mod tensor`）は API サーフェスを変更しない。
        // 実際の内容変更は diff 内のモジュール自身のファイルから検出される。
        if matches!(sym.kind.as_str(), "impl" | "module")
            || (sym.kind == "type"
                && (self.lang_id != LangId::Rust || !self.rust_alias_changes.contains(&sym.name)))
        {
            return false;
        }
        // Rust の type はモジュール直下の type alias のときだけ判定対象にする
        // (impl / trait の関連型は名前だけの cross-file 探索に流さない)。
        let overlapping = if self.lang_id == LangId::Rust && sym.kind == "type" {
            Some(original)
                .filter(|candidate| declaration::rust_module_alias(self.root, candidate).is_some())
        } else {
            Some(original)
        };
        // 2. テストコンテキスト内のシンボルをスキップ
        if overlapping.is_some_and(|s| {
            is_in_test_context(
                self.root,
                self.source,
                &s.range,
                self.lang_id,
                self.file_path,
            )
        }) {
            return false;
        }
        // 3. ボディのみの変更の関数/メソッドをスキップ
        if (sym.kind == "function" || sym.kind == "method")
            && !self.sig_changes.iter().any(|sc| sc.name == sym.name)
            && !(self.lang_id == LangId::Rust && self.rust_kind_changes.contains(&sym.name))
        {
            return false;
        }
        // 3a. Kotlin/Java/Swift/TS/C# の `override` メソッドは親 interface/class から
        // 呼ばれるため cross-file caller を追跡できない。親 API のシグネチャは不変なので
        // 下流互換性にも影響せず、本体変更は impl 変更として扱い api.mod から除外する。
        if (sym.kind == "function" || sym.kind == "method")
            && overlapping.is_some_and(|s| {
                symbols::is_override_method(self.root, self.source, self.lang_id, &s.range)
            })
        {
            return false;
        }
        // 3b. 宣言ヘッダ行が変更されていない型シンボルをスキップ。
        // 型宣言ノードの開始行 (= シンボル range の開始行) が新側変更行に含まれない場合は、
        // body 内の変更や、名前を言及する docblock コメント (`* class Foo`) / 文字列の変更が
        // あっても他ファイルのクラス参照へ伝播しない。docblock コメントは AST 上の型宣言ノードに
        // 含まれない別ノードなので、宣言開始行と変更行の交差で判定すればコメント/文字列の名前
        // 言及を確実に除外できる (GitLab #35: クラス参照の過剰列挙、テキスト照合の誤マッチを排除)。
        // 宣言開始行のみを見るため `trait GuestMemory` のような単一行ヘッダにも、フリー関数の
        // シグネチャ行に型名が出現するだけのケースにも正しく作用する。
        if matches!(
            sym.kind.as_str(),
            "trait" | "struct" | "class" | "interface" | "enum"
        ) && let Some(s) = overlapping
            && !self.changed_new_lines.contains(&s.range.start.line)
        {
            return false;
        }
        // 3c. オブジェクトリテラル変数の「メンバー追加のみ」変更をスキップ。
        // `export const ns = { fnA, fnB }` 形式のファサードへ新メンバーを追加しても、
        // 既存メンバーだけを使う参照側は壊れない (追加は後方互換)。宣言ヘッダ行が不変で、
        // シンボル range 内の変更に削除 (`-` 行) を含まない場合は cross-file 検索から外す。
        // メンバー削除・書き換え (`-` 行が range 内に出る) は従来どおり blocking 側に残す
        // (Issue 2026-07-27-namespace-object-impact-granularity)。
        if sym.kind == "variable"
            && let Some(s) = overlapping
            && !self.changed_new_lines.contains(&s.range.start.line)
            && is_js_ts_object_literal_variable(self.root, self.lang_id, &s.range)
            && !crate::engine::diff::has_deletion_in_new_range(
                self.diff_input,
                self.file_path,
                s.range.start.line,
                s.range.end.line,
            )
        {
            return false;
        }
        // 4. エクスポートされていないシンボルをスキップ
        if !overlapping.is_some_and(|s| {
            (self.lang_id == LangId::Rust && declaration::is_rust_file_root_symbol(self.root, s))
                || (self.lang_id == LangId::Rust
                    && self.rust_private_method_owners.contains_key(&s.name)
                    && declaration::rust_private_inherent_method(self.root, self.source, s)
                        .is_some())
                || symbols::is_symbol_exported(
                    self.root,
                    self.source,
                    self.lang_id,
                    s.identity_range(),
                )
        }) {
            return false;
        }
        // 5. 変更行にシンボル名が出現しない場合スキップ。
        // ただしシグネチャ変更が検出済みのシンボルは除外しない。複数行の引数リストへの
        // 引数追加 (`+    b: u32,`) は変更行に名前が出ないが、宣言ヘッダの比較で呼び出し
        // 契約の変更と確定している (`declaration::reconcile_declaration_changes`)。
        // JS/TS のオブジェクトリテラル変数で範囲内に削除 (メンバーの削除・値の書き換え) が
        // あるものも除外しない。複数行のオブジェクトは名前が宣言ヘッダ行にしか無く、メンバーの
        // 変更行には出ないため、名前の有無では呼び出し元への影響を判定できない
        // (メンバー追加だけの変更は 3c で除外済み。`as const` / `satisfies` / 括弧で包んだ
        // オブジェクトも同じ理由で除外しない)。
        // 初期値だけを変えた非公開の Rust の const / static も除外しない。名前の有無は同じ
        // ファイルのほかの差分 (テストの追加など) で変わるので使わず、参照の位置で振り分ける
        // (パターンや配列長などの値をコンパイル時に使う参照は blocking、関数の本体で読むだけの
        // 参照は情報提供。`rust_const_value`)。
        if !self.sig_changes.iter().any(|sc| sc.name == sym.name)
            && !self.rust_alias_changes.contains(&sym.name)
            && !self.rust_value_only_constants.contains_key(&sym.name)
            && !(sym.kind == "variable"
                && overlapping.is_some_and(|s| {
                    is_js_ts_wrapped_object_literal_variable(self.root, self.lang_id, &s.range)
                        // 削除はオブジェクトの範囲の内側に限る (直前の別の宣言の削除は含めない。
                        // affected の判定と同じ規約)。
                        && super::symbol_range_has_deletion(
                            s.range.start.line,
                            s.range.end.line,
                            self.deletion_gaps,
                        )
                }))
            && !is_symbol_in_changed_lines(self.diff_input, self.file_path, &sym.name, self.lang_id)
        {
            return false;
        }
        // 6. 新規追加シンボル (change_type == "added") は cross-file caller がまだない
        //    ため検索対象から除外する。同一 commit 内で追加された caller は他ファイルの
        //    シンボル変更経由で別途検出されるため、本シンボル単独の cross-file 探索は
        //    ノイズだけが残る。新規ファイルの全シンボルがここで除外される。
        if sym.change_type == "added" {
            return false;
        }
        true
    }
}

/// variable シンボルの range (variable_declarator 全体) が JS/TS のオブジェクトリテラル
/// 初期化子 (`const ns = { ... }`) を持つか判定する。フィルタ 3c の適用対象判定に使う。
/// 対象言語外・ノード不一致・value 欠落はすべて false (= 従来どおり cross-file 検索) に倒す。
///
/// `as const` / `satisfies` 付き (`const ns = { ... } as const;`) は value が `as_expression` /
/// `satisfies_expression` になるため false になり、フィルタは適用されず従来検索に落ちる。
/// 検出漏れ側には倒れないため現状は unwrap しない。
fn is_js_ts_object_literal_variable(
    root: tree_sitter::Node<'_>,
    lang_id: LangId,
    range: &crate::models::location::Range,
) -> bool {
    js_ts_variable_value(root, lang_id, range).is_some_and(|v| v.kind() == "object")
}

/// `is_js_ts_object_literal_variable` と同じ判定を、`as const` / `satisfies` / 冗長括弧で
/// 包んだ初期化子 (`const ns = { ... } as const;`) にも広げたもの。フィルタ 5 の除外を
/// 外す側だけで使う (3c の「追加のみ」除外の対象は広げない)。既知の包みだけを外し、
/// それ以外の式の中にあるオブジェクトは探さない。
fn is_js_ts_wrapped_object_literal_variable(
    root: tree_sitter::Node<'_>,
    lang_id: LangId,
    range: &crate::models::location::Range,
) -> bool {
    let mut value = js_ts_variable_value(root, lang_id, range);
    while let Some(node) = value {
        match node.kind() {
            "object" => return true,
            "parenthesized_expression" | "as_expression" | "satisfies_expression" => {
                value = node.named_child(0);
            }
            _ => return false,
        }
    }
    false
}

/// variable シンボルの range (variable_declarator 全体) から JS/TS の初期化子ノードを返す。
/// 対象言語外・ノード不一致・value 欠落は `None`。
fn js_ts_variable_value<'a>(
    root: tree_sitter::Node<'a>,
    lang_id: LangId,
    range: &crate::models::location::Range,
) -> Option<tree_sitter::Node<'a>> {
    if !matches!(
        lang_id,
        LangId::Javascript | LangId::Typescript | LangId::Tsx
    ) {
        return None;
    }
    let start = tree_sitter::Point {
        row: range.start.line,
        column: range.start.column,
    };
    let end = tree_sitter::Point {
        row: range.end.line,
        column: range.end.column,
    };
    // variable シンボルの range は variable_declarator 全体を指す。念のため祖先方向にも辿る。
    let mut cur = root.descendant_for_point_range(start, end);
    while let Some(n) = cur {
        if n.kind() == "variable_declarator" {
            return n.child_by_field_name("value");
        }
        cur = n.parent();
    }
    None
}

/// 同一ファイル判定。
///
/// 参照側 (`refs` の走査結果を `--dir` で strip したパス) も変更側 (`--relative` 付き
/// `git diff` 由来の new_path) も `--dir` 相対なので完全一致で判定する。
/// 旧実装は `ref_path.ends_with("/{source_path}")` も許していたため、`src/util.ts` を
/// 変更すると `packages/app/src/util.ts` の呼び出し側まで「同じファイル内の参照」として
/// 捨てていた (見逃し)。後方一致は参照パスが絶対パスだった頃の名残で、現在の経路では
/// 参照パスは常に `--dir` 相対になる。
pub(super) fn is_same_source_file(ref_path: &str, source_path: &str) -> bool {
    ref_path == source_path
}

/// 参照のある文が import/re-export 文か (`import_context_at` の結果)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ImportContext {
    NotImport,
    Import,
    /// `= require(` / `require(` の規則だけで import と判定した文。行のテキストに require が
    /// あるだけなので、同じ文の実際の呼び出し (`require('./a').foo(1)`) まで含んでしまう。
    /// JS/TS では AST で CommonJS の束縛位置かを確かめ直す (`is_commonjs_require_binding`)。
    RequireOnly,
}

/// 参照のコンテキスト行が import/re-export 文かどうかを判定する (require の規則だけで
/// 当たる文も import とみなす。テストで規則を確かめるための入口)。
#[cfg(test)]
fn is_import_context(context: Option<&str>) -> bool {
    context.is_some_and(|ctx| import_context_kind(ctx.trim()) != ImportContext::NotImport)
}

fn import_context_kind(ctx: &str) -> ImportContext {
    if is_import_statement_text(ctx) {
        ImportContext::Import
    } else if ctx.contains("= require(") || ctx.starts_with("require(") {
        // JS/TS: const { X } = require('...'), require('...')
        ImportContext::RequireOnly
    } else {
        ImportContext::NotImport
    }
}

/// require 以外の規則で import/re-export 文と言えるか。
fn is_import_statement_text(ctx: &str) -> bool {
    // JS/TS: import { X } from '...', import X from '...'
    if ctx.starts_with("import ") || ctx.starts_with("import{") {
        return true;
    }
    // JS/TS: export { X } from '...', export * from '...'
    if (ctx.starts_with("export ") || ctx.starts_with("export{"))
        && (ctx.contains(" from ") || ctx.contains(" from\"") || ctx.contains(" from'"))
    {
        return true;
    }
    // JS/TS: import 済みシンボルを公開し直す barrel 形式
    if is_js_ts_export_clause(ctx) {
        return true;
    }
    // Python: from module import X
    if ctx.starts_with("from ") && ctx.contains(" import ") {
        return true;
    }
    // Rust: use crate::..., pub use ...
    if ctx.starts_with("use ") || ctx.starts_with("pub use ") {
        return true;
    }
    // Go: import "..."
    // Go は個別シンボルを import しないため通常は該当しないが念のため
    if ctx.starts_with("import (") || ctx.starts_with("import \"") {
        return true;
    }
    // Ruby: require, require_relative
    if ctx.starts_with("require ") || ctx.starts_with("require_relative ") {
        return true;
    }
    // C/C++: #include "..." / #include <...>
    if ctx.starts_with("#include ") {
        return true;
    }
    // C#: using System; / using static ...
    // "using var" / "using (" はリソース管理（import ではない）
    if ctx.starts_with("using ")
        && ctx.ends_with(';')
        && !ctx.starts_with("using var ")
        && !ctx.starts_with("using (")
    {
        return true;
    }
    // Zig: const std = @import("std");
    if ctx.contains("@import(") {
        return true;
    }
    // Java/Kotlin/Swift/PHP: すでにカバー済み
    // ("import " / "use " で捕捉)
    false
}

/// 同一行に複数文がある場合、参照列を含む文だけで import/re-export 文脈を判定する。
pub(super) fn import_context_at(context: Option<&str>, column: usize) -> ImportContext {
    let Some(ctx) = context else {
        return ImportContext::NotImport;
    };
    import_context_kind(statement_segment_at_column(ctx, column).trim())
}

/// JS/TS の参照ノードが CommonJS の import の束縛位置にあるか。
///
/// import とみなすのは次の 2 つだけ (どちらも括弧は透過する):
/// - `require(...)` を初期化子に持つ変数宣言の束縛パターン (`const { foo } = require('./a')`
///   の束縛名・`{ foo: bar }` の取り出すキー・入れ子・既定値付き・rest)。既定値の式と
///   computed key の式はパターン内でも実行される式なので含めない
///   (`const { foo = foo(1) } = ...` の 2 つ目の `foo` は呼び出し)。
/// - 初期化子そのものが `require(...).foo` の変数宣言の `foo` (`const foo = require('./a').foo`)。
///
/// それ以外 (`require('./a').foo(1)` の呼び出し、同じ宣言の別の declarator、代入式
/// `x = require('./a').foo`、`require('./a').sub` の分割代入など) は import と証明しない
/// (呼び出し元として blocking 側に残す)。callee は素の `require` 識別子に限る。
pub(super) fn is_commonjs_require_binding(node: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    let is_field = |parent: tree_sitter::Node<'_>, field: &str, child: tree_sitter::Node<'_>| {
        parent
            .child_by_field_name(field)
            .is_some_and(|n| n.id() == child.id())
    };
    // `const foo = require('./a').foo;`
    if let Some(member) = node.parent()
        && member.kind() == "member_expression"
        && is_field(member, "property", node)
        && member
            .child_by_field_name("object")
            .is_some_and(|object| is_require_call(unwrap_parens(object), source))
    {
        let mut value = member;
        while let Some(parent) = value.parent()
            && parent.kind() == "parenthesized_expression"
        {
            value = parent;
        }
        return value.parent().is_some_and(|declarator| {
            declarator.kind() == "variable_declarator" && is_field(declarator, "value", value)
        });
    }
    // 束縛パターンを宣言まで辿る。実行される式 (既定値・computed key) へ入った時点で証明しない。
    let mut child = node;
    while let Some(parent) = child.parent() {
        match parent.kind() {
            "object_pattern" | "array_pattern" | "rest_pattern" => {}
            "object_assignment_pattern" | "assignment_pattern" => {
                if !is_field(parent, "left", child) {
                    return false;
                }
            }
            "pair_pattern" => {
                // 取り出すキーは静的な名前だけ (computed key は式)。
                if is_field(parent, "key", child) {
                    if child.kind() == "computed_property_name" {
                        return false;
                    }
                } else if !is_field(parent, "value", child) {
                    return false;
                }
            }
            "variable_declarator" => {
                return is_field(parent, "name", child)
                    && parent
                        .child_by_field_name("value")
                        .is_some_and(|value| is_require_call(unwrap_parens(value), source));
            }
            _ => return false,
        }
        child = parent;
    }
    false
}

/// 冗長な括弧を外す。
fn unwrap_parens(mut node: tree_sitter::Node<'_>) -> tree_sitter::Node<'_> {
    while node.kind() == "parenthesized_expression"
        && let Some(inner) = node.named_child(0)
    {
        node = inner;
    }
    node
}

/// 素の `require` 識別子を callee とする呼び出し (`require('./a')`) か。
fn is_require_call(node: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    node.kind() == "call_expression"
        && node.child_by_field_name("function").is_some_and(|callee| {
            callee.kind() == "identifier" && callee.utf8_text(source).ok() == Some("require")
        })
}

fn statement_segment_at_column(ctx: &str, column: usize) -> &str {
    let mut col = column.min(ctx.len());
    while col > 0 && !ctx.is_char_boundary(col) {
        col -= 1;
    }
    let start = ctx[..col].rfind(';').map_or(0, |ix| ix + 1);
    let end = ctx[col..].find(';').map_or(ctx.len(), |ix| col + ix);
    &ctx[start..end]
}

fn is_js_ts_export_clause(ctx: &str) -> bool {
    let Some(rest) = ctx.strip_prefix("export") else {
        return false;
    };
    let rest = rest.trim_start();
    if rest.starts_with('{') {
        return true;
    }
    if let Some(type_rest) = rest.strip_prefix("type") {
        return type_rest.trim_start().starts_with('{');
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::impact::HunkInfo;

    // --- CrossFileFilterContext::should_include_for_cross_file ---

    /// `src/lib.rs` の 1 行目 (0-indexed 0) を書き換えた unified diff を作る。
    fn one_line_diff(new_line: &str) -> String {
        format!("--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,1 +1,1 @@\n-// old\n+{new_line}\n",)
    }

    fn affected(name: &str, kind: &str, change_type: &str) -> AffectedSymbol {
        AffectedSymbol {
            name: name.to_string(),
            kind: kind.to_string(),
            change_type: change_type.to_string(),
        }
    }

    /// Pass 1 と同じく、hunk に重なるシンボルを affected の元のシンボルとして選ぶ
    /// (これらのテストの fixture では名前が一意)。該当が無い名前はフィルタ 1 で
    /// シンボルを見る前に除外されるケースなので、先頭のシンボルで代用する。
    fn original_for<'a>(syms: &'a [Symbol], name: &str, hunks: &[HunkInfo]) -> &'a Symbol {
        syms.iter()
            .find(|s| s.name == name && super::super::symbol_overlaps_hunks(s, hunks))
            .or_else(|| syms.first())
            .expect("fixture has at least one symbol")
    }

    /// 1 行目全体を hunk とみなす per-file 定数を組み立ててから判定を実行する。
    fn include_for_rust(
        source: &str,
        sym: &AffectedSymbol,
        sig_changes: &[SignatureChange],
        changed_lines: &[usize],
    ) -> bool {
        include_for_rust_with_alias_changes(source, sym, sig_changes, changed_lines, &[])
    }

    fn include_for_rust_with_alias_changes(
        source: &str,
        sym: &AffectedSymbol,
        sig_changes: &[SignatureChange],
        changed_lines: &[usize],
        alias_changes: &[&str],
    ) -> bool {
        let bytes = source.as_bytes();
        let tree = crate::engine::parser::parse_source(bytes, LangId::Rust).expect("parse");
        let root = tree.root_node();
        let syms = symbols::extract_symbols(root, bytes, LangId::Rust).expect("symbols");
        let hunks = vec![HunkInfo {
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 1,
        }];
        let diff_input = one_line_diff(source.lines().next().unwrap_or_default());
        let changed_new_lines: HashSet<usize> = changed_lines.iter().copied().collect();
        let deletion_gaps =
            crate::engine::diff::extract_changed_line_facts(&diff_input, "src/lib.rs")
                .deletion_gaps;
        let rust_alias_changes: HashSet<String> = alias_changes
            .iter()
            .map(|name| (*name).to_string())
            .collect();
        let ctx = CrossFileFilterContext {
            rust_kind_changes: &HashSet::new(),
            rust_alias_changes: &rust_alias_changes,
            rust_value_only_constants: &HashMap::new(),
            rust_private_method_owners: &HashMap::new(),
            sig_changes,
            diff_input: &diff_input,
            file_path: "src/lib.rs",
            root,
            source: bytes,
            lang_id: LangId::Rust,
            changed_new_lines: &changed_new_lines,
            deletion_gaps: &deletion_gaps,
        };
        ctx.should_include_for_cross_file(sym, original_for(&syms, &sym.name, &hunks))
    }

    fn sig_change(name: &str) -> SignatureChange {
        SignatureChange {
            name: name.to_string(),
            old_signature: "fn foo()".to_string(),
            new_signature: "fn foo(a: usize)".to_string(),
        }
    }

    // 1. impl owner / モジュール宣言は AST を見る前に除外される
    #[test]
    fn cross_file_filter_skips_impl_and_module_kind() {
        let source = "pub fn foo(a: usize) -> usize { a }\n";
        assert!(!include_for_rust(
            source,
            &affected("Foo", "impl", "modified"),
            &[sig_change("Foo")],
            &[0]
        ));
        assert!(!include_for_rust(
            source,
            &affected("foo", "module", "modified"),
            &[sig_change("foo")],
            &[0]
        ));
    }

    #[test]
    fn cross_file_filter_includes_rust_type_alias_without_pub() {
        assert!(!include_for_rust(
            "pub type Id = u64;\n",
            &affected("Id", "type", "modified"),
            &[],
            &[0]
        ));
        for source in [
            "type Id = u64;\n",
            "pub(crate) type Id = u64;\n",
            "pub type Id = u64;\n",
        ] {
            assert!(include_for_rust_with_alias_changes(
                source,
                &affected("Id", "type", "modified"),
                &[],
                &[0],
                &["Id"]
            ));
        }
    }

    // シグネチャ変更のある exported fn は cross-file 検索に含める
    #[test]
    fn cross_file_filter_includes_exported_fn_with_signature_change() {
        let source = "pub fn foo(a: usize) -> usize { a }\n";
        assert!(include_for_rust(
            source,
            &affected("foo", "function", "modified"),
            &[sig_change("foo")],
            &[0]
        ));
    }

    // 3. シグネチャ変更が無い (= ボディのみ変更) 関数は除外
    #[test]
    fn cross_file_filter_skips_body_only_change() {
        let source = "pub fn foo(a: usize) -> usize { a }\n";
        assert!(!include_for_rust(
            source,
            &affected("foo", "function", "modified"),
            &[],
            &[0]
        ));
    }

    #[test]
    fn cross_file_filter_skips_private_function_inside_inline_module() {
        let source = "mod inner { fn foo(a: usize) -> usize { a } }\n";
        assert!(!include_for_rust(
            source,
            &affected("foo", "function", "modified"),
            &[sig_change("foo")],
            &[0]
        ));
    }

    // 6. 新規追加シンボルは cross-file caller がまだ無いため除外
    #[test]
    fn cross_file_filter_skips_added_symbol() {
        let source = "pub fn foo(a: usize) -> usize { a }\n";
        assert!(!include_for_rust(
            source,
            &affected("foo", "function", "added"),
            &[sig_change("foo")],
            &[0]
        ));
    }

    /// TS ソース + 指定 diff で `should_include_for_cross_file` を評価する (3c フィルタ用)。
    fn include_for_ts(
        source: &str,
        sym: &AffectedSymbol,
        diff_input: &str,
        changed_lines: &[usize],
        hunks: Vec<HunkInfo>,
    ) -> bool {
        let bytes = source.as_bytes();
        let tree = crate::engine::parser::parse_source(bytes, LangId::Typescript).expect("parse");
        let root = tree.root_node();
        let syms = symbols::extract_symbols(root, bytes, LangId::Typescript).expect("symbols");
        let changed_new_lines: HashSet<usize> = changed_lines.iter().copied().collect();
        let deletion_gaps =
            crate::engine::diff::extract_changed_line_facts(diff_input, "mod.ts").deletion_gaps;
        let ctx = CrossFileFilterContext {
            rust_kind_changes: &HashSet::new(),
            rust_alias_changes: &HashSet::new(),
            rust_value_only_constants: &HashMap::new(),
            rust_private_method_owners: &HashMap::new(),
            sig_changes: &[],
            diff_input,
            file_path: "mod.ts",
            root,
            source: bytes,
            lang_id: LangId::Typescript,
            changed_new_lines: &changed_new_lines,
            deletion_gaps: &deletion_gaps,
        };
        ctx.should_include_for_cross_file(sym, original_for(&syms, &sym.name, &hunks))
    }

    // 3c. オブジェクトリテラル変数への「メンバー追加のみ」変更は cross-file 検索から除外
    #[test]
    fn cross_file_filter_skips_object_literal_member_addition_only() {
        let source = "export function alpha(): number {\n  return 1;\n}\nexport const api = {\n  alpha,\n  gamma,\n};\n";
        let diff = "--- a/mod.ts\n+++ b/mod.ts\n@@ -4,3 +4,4 @@\n export const api = {\n   alpha,\n+  gamma,\n };\n";
        let hunks = vec![HunkInfo {
            old_start: 4,
            old_count: 3,
            new_start: 4,
            new_count: 4,
        }];
        assert!(!include_for_ts(
            source,
            &affected("api", "variable", "modified"),
            diff,
            &[5],
            hunks
        ));
    }

    // 3c. メンバーの書き換え (`-` 行が range 内) を含む場合は従来どおり cross-file 検索に含める
    #[test]
    fn cross_file_filter_keeps_object_literal_member_rewrite() {
        let source = "export function alpha(): number {\n  return 1;\n}\nexport const api = {\n  alpha,\n  beta: betaV2, // api member rewrite\n};\n";
        let diff = "--- a/mod.ts\n+++ b/mod.ts\n@@ -4,4 +4,4 @@\n export const api = {\n   alpha,\n-  beta,\n+  beta: betaV2, // api member rewrite\n };\n";
        let hunks = vec![HunkInfo {
            old_start: 4,
            old_count: 4,
            new_start: 4,
            new_count: 4,
        }];
        assert!(include_for_ts(
            source,
            &affected("api", "variable", "modified"),
            diff,
            &[5],
            hunks
        ));
    }

    // 5. 複数行のオブジェクトリテラル変数は、名前が変更行に無くてもメンバーの削除・値の書き換えが
    // あれば除外しない (名前が宣言ヘッダ行にしか無いため、フィルタ 5 では判定できない)。
    // `as const` で包んだものも同じ。対照: メンバーを足しただけの変更は従来どおり除外する
    // (包んでいないものは 3c、包んだものはフィルタ 5)。削除はオブジェクトの範囲の内側に限り、
    // 直前の別の宣言を消しただけの削除では除外を外さない。
    #[test]
    fn cross_file_filter_keeps_multi_line_object_member_removal_without_name_on_changed_lines() {
        // (ラベル, 変更後のメンバー行, diff の hunk (ヘッダ込み), cross-file 検索に含めるか)
        let cases = [
            (
                "末尾のメンバー削除",
                "  alpha: 1,\n",
                "@@ -1,4 +1,3 @@\n export const api = {\n   alpha: 1,\n-  beta: 2,\n }SUFFIX;\n",
                true,
            ),
            (
                "先頭のメンバー削除",
                "  alpha: 1,\n",
                "@@ -1,4 +1,3 @@\n export const api = {\n-  beta: 2,\n   alpha: 1,\n }SUFFIX;\n",
                true,
            ),
            (
                "唯一のメンバー削除",
                "",
                "@@ -1,3 +1,2 @@\n export const api = {\n-  beta: 2,\n }SUFFIX;\n",
                true,
            ),
            (
                "-U0 のメンバー削除",
                "  alpha: 1,\n",
                "@@ -3 +2,0 @@\n-  beta: 2,\n",
                true,
            ),
            (
                "値の書き換え",
                "  alpha: 2,\n",
                "@@ -1,3 +1,3 @@\n export const api = {\n-  alpha: 1,\n+  alpha: 2,\n }SUFFIX;\n",
                true,
            ),
            (
                "対照: メンバー追加のみ",
                "  alpha: 1,\n  beta: 2,\n",
                "@@ -1,3 +1,4 @@\n export const api = {\n   alpha: 1,\n+  beta: 2,\n }SUFFIX;\n",
                false,
            ),
            (
                "対照: メンバー追加 + 直前の別の宣言の削除",
                "  alpha: 1,\n  beta: 2,\n",
                "@@ -1,4 +1,4 @@\n-export const before = 1;\n export const api = {\n   alpha: 1,\n+  beta: 2,\n }SUFFIX;\n",
                false,
            ),
        ];
        for suffix in ["", " as const"] {
            for (label, members, hunk, expected) in cases {
                let source = format!("export const api = {{\n{members}}}{suffix};\n");
                let diff = format!(
                    "--- a/mod.ts\n+++ b/mod.ts\n{}",
                    hunk.replace("SUFFIX", suffix)
                );
                let hunks = crate::engine::diff::parse_unified_diff(&diff)
                    .into_iter()
                    .next()
                    .expect("diff file")
                    .hunks;
                let added: Vec<usize> =
                    crate::engine::diff::extract_changed_line_facts(&diff, "mod.ts")
                        .added_lines
                        .into_iter()
                        .collect();
                assert_eq!(
                    include_for_ts(
                        &source,
                        &affected("api", "variable", "modified"),
                        &diff,
                        &added,
                        hunks
                    ),
                    expected,
                    "{label} (suffix={suffix:?})\n{diff}"
                );
            }
        }
    }

    // 3c. 宣言ヘッダ行自体が変更された場合は除外しない (署名変更の可能性)
    #[test]
    fn cross_file_filter_keeps_object_literal_with_changed_header() {
        let source = "export function alpha(): number {\n  return 1;\n}\nexport const api = {\n  alpha,\n  gamma,\n};\n";
        let diff = "--- a/mod.ts\n+++ b/mod.ts\n@@ -4,3 +4,4 @@\n-export const api2 = {\n+export const api = {\n   alpha,\n+  gamma,\n };\n";
        let hunks = vec![HunkInfo {
            old_start: 4,
            old_count: 3,
            new_start: 4,
            new_count: 4,
        }];
        assert!(include_for_ts(
            source,
            &affected("api", "variable", "modified"),
            diff,
            &[3, 5],
            hunks
        ));
    }

    // 5. 変更行に名前が出ないシンボルでも、シグネチャ変更が確定していれば除外しない。
    // 複数行の引数リストへの引数追加 (`+  b: number,`) は名前を含まないが、宣言ヘッダの比較で
    // 呼び出し契約の変更と確定している (`declaration::reconcile_declaration_changes`)。
    #[test]
    fn cross_file_filter_keeps_signature_change_whose_name_is_not_on_changed_lines() {
        let source = "export const arrow = (\n  a: number,\n  b: number,\n): number => a;\n";
        let diff = "--- a/mod.ts\n+++ b/mod.ts\n@@ -1,3 +1,4 @@\n export const arrow = (\n   a: number,\n+  b: number,\n ): number => a;\n";
        let bytes = source.as_bytes();
        let tree = crate::engine::parser::parse_source(bytes, LangId::Typescript).expect("parse");
        let root = tree.root_node();
        let syms = symbols::extract_symbols(root, bytes, LangId::Typescript).expect("symbols");
        let hunks = vec![HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 4,
        }];
        let changed_new_lines: HashSet<usize> = HashSet::from([2]);
        let deletion_gaps = HashSet::new();
        let include = |sig_changes: &[SignatureChange]| {
            CrossFileFilterContext {
                rust_kind_changes: &HashSet::new(),
                rust_alias_changes: &HashSet::new(),
                rust_value_only_constants: &HashMap::new(),
                rust_private_method_owners: &HashMap::new(),
                sig_changes,
                diff_input: diff,
                file_path: "mod.ts",
                root,
                source: bytes,
                lang_id: LangId::Typescript,
                changed_new_lines: &changed_new_lines,
                deletion_gaps: &deletion_gaps,
            }
            .should_include_for_cross_file(
                &affected("arrow", "variable", "modified"),
                original_for(&syms, "arrow", &hunks),
            )
        };
        assert!(include(&[sig_change("arrow")]));
        // 対照: シグネチャ変更が無ければ従来どおり「名前が変更行に無い」で除外する
        // (本体だけの変更を cross-file 検索に載せない既存の抑制)。
        assert!(!include(&[]));
    }

    /// 初期値だけを変えた非公開の Rust の static は、変更行に名前が無くても cross-file 検索に
    /// 載せる (Issue #257)。名前の有無は同じファイルのほかの差分で変わるので、参照の位置で
    /// 振り分ける。
    #[test]
    fn cross_file_filter_keeps_rust_value_only_constant_whose_name_is_not_on_changed_lines() {
        let source =
            "pub(super) static GREETING: Note = Note {\n    text: \"Hello, world.\",\n};\n";
        let diff = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n pub(super) static GREETING: Note = Note {\n-    text: \"Hello.\",\n+    text: \"Hello, world.\",\n };\n";
        let bytes = source.as_bytes();
        let tree = crate::engine::parser::parse_source(bytes, LangId::Rust).expect("parse");
        let root = tree.root_node();
        let syms = symbols::extract_symbols(root, bytes, LangId::Rust).expect("symbols");
        let hunks = vec![HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 3,
        }];
        let changed_new_lines: HashSet<usize> = HashSet::from([1]);
        let deletion_gaps =
            crate::engine::diff::extract_changed_line_facts(diff, "src/lib.rs").deletion_gaps;
        let include = |value_only: &HashMap<String, declaration::RustValueOnlyConstant>| {
            CrossFileFilterContext {
                rust_kind_changes: &HashSet::new(),
                rust_alias_changes: &HashSet::new(),
                rust_value_only_constants: value_only,
                rust_private_method_owners: &HashMap::new(),
                sig_changes: &[],
                diff_input: diff,
                file_path: "src/lib.rs",
                root,
                source: bytes,
                lang_id: LangId::Rust,
                changed_new_lines: &changed_new_lines,
                deletion_gaps: &deletion_gaps,
            }
            .should_include_for_cross_file(
                &affected("GREETING", "constant", "modified"),
                original_for(&syms, "GREETING", &hunks),
            )
        };
        let proven = HashMap::from([(
            "GREETING".to_owned(),
            declaration::RustValueOnlyConstant {
                is_static: true,
                type_names: ["Note".to_owned()].into(),
            },
        )]);
        assert!(include(&proven));
        // 対照: 証明が無ければ従来どおり「名前が変更行に無い」で除外する。
        assert!(!include(&HashMap::new()));
    }

    // 3b. 宣言ヘッダ行が変更行に含まれない型シンボルは除外 (body/コメントのみ変更)
    #[test]
    fn cross_file_filter_skips_struct_with_unchanged_header() {
        let source = "pub struct Foo {\n    pub a: usize,\n}\n";
        assert!(include_for_rust(
            source,
            &affected("Foo", "struct", "modified"),
            &[],
            &[0]
        ));
        assert!(!include_for_rust(
            source,
            &affected("Foo", "struct", "modified"),
            &[],
            &[1]
        ));
    }

    // --- is_same_source_file ---

    #[test]
    fn same_source_file_exact_match() {
        assert!(is_same_source_file("src/main.rs", "src/main.rs"));
    }

    /// 変更ファイルと末尾が一致するだけの別ファイル (`packages/app/src/util.ts` と
    /// `src/util.ts`) は同一ファイルではない。旧実装はパス区切り付きの後方一致で同一扱いし、
    /// monorepo で同名パスを持つパッケージの呼び出し側を捨てていた。
    #[test]
    fn same_source_file_rejects_path_suffix_match() {
        assert!(!is_same_source_file(
            "packages/app/src/util.ts",
            "src/util.ts"
        ));
        assert!(!is_same_source_file("other/src/main.rs", "src/main.rs"));
        // 対照: 完全一致は引き続き同一ファイル。
        assert!(is_same_source_file("src/util.ts", "src/util.ts"));
    }

    #[test]
    fn same_source_file_different_similar_suffix() {
        assert!(!is_same_source_file("test_main.rs", "main.rs"));
    }

    // --- is_import_context ---

    #[test]
    fn import_context_ts_import() {
        assert!(is_import_context(Some(
            "import { useCommitStore } from '../stores'"
        )));
        assert!(is_import_context(Some(
            "import useCommitStore from '../stores'"
        )));
        assert!(is_import_context(Some(
            "import{ useCommitStore } from '../stores'"
        )));
    }

    #[test]
    fn import_context_ts_reexport() {
        assert!(is_import_context(Some(
            "export { useCommitStore } from '../stores'"
        )));
        assert!(is_import_context(Some(
            "export{ useCommitStore } from './commitStore'"
        )));
        assert!(is_import_context(Some("export { useCommitStore };")));
        assert!(is_import_context(Some("export{ useCommitStore };")));
        assert!(is_import_context(Some("export type { CommitStore };")));
    }

    #[test]
    fn import_context_at_uses_statement_containing_column() {
        let line = "export { useCommitStore }; export const value = useCommitStore();";
        let reexport_col = line.find("useCommitStore").expect("re-export ref");
        let call_col = line.rfind("useCommitStore").expect("call ref");

        assert_eq!(
            import_context_at(Some(line), reexport_col),
            ImportContext::Import
        );
        assert_eq!(
            import_context_at(Some(line), call_col),
            ImportContext::NotImport
        );
    }

    /// `= require(` / `require(` の規則だけで import と判定した文は `RequireOnly` として区別する
    /// (呼び出し側が JS/TS では AST で束縛位置かを確かめ直す)。他の規則に当たる文は従来どおり。
    #[test]
    fn import_context_distinguishes_require_only_statements() {
        for line in [
            "const { foo } = require('./a');",
            "const value = require('./a').foo(1);",
            "require('./a').foo(1);",
        ] {
            assert_eq!(
                import_context_at(Some(line), line.find("foo").unwrap()),
                ImportContext::RequireOnly,
                "{line}"
            );
            assert!(is_import_context(Some(line)), "{line}");
        }
        for line in ["import foo = require('./a');", "import { foo } from './a';"] {
            assert_eq!(
                import_context_at(Some(line), line.find("foo").unwrap()),
                ImportContext::Import,
                "{line}"
            );
        }
        // Ruby の require (括弧なし) は従来どおり import。
        assert_eq!(
            import_context_at(Some("require 'commit_store'"), 0),
            ImportContext::Import
        );
    }

    /// JS/TS の参照ノードが CommonJS の束縛位置か。束縛パターン内でも既定値・computed key の
    /// 式は実行されるので import ではない。
    #[test]
    fn commonjs_require_binding_accepts_only_binding_positions() {
        // (ソース, 対象の `foo` の出現番号 (0 始まり), import とみなすか)
        let cases = [
            ("const { foo } = require('./a');", 0, true),
            ("const { foo: bar } = require('./a');", 0, true),
            ("const { foo = 1 } = require('./a');", 0, true),
            ("const { a: { foo } } = require('./a');", 0, true),
            ("const { ...foo } = require('./a');", 0, true),
            ("const foo = require('./a').foo;", 1, true),
            ("const foo = (require('./a')).foo;", 1, true),
            ("const { foo } = (require('./a'));", 0, true),
            // 呼び出し・他の式の中の参照は import ではない。
            ("const value = require('./a').foo(1);", 0, false),
            ("require('./a').foo(1);", 0, false),
            ("const m = require('./m'), r = foo(1);", 0, false),
            ("const { bar = foo(1) } = require('./a');", 0, false),
            ("const { [foo(1)]: value } = require('./a');", 0, false),
            ("let x; x = require('./a').foo;", 0, false),
            ("exports.foo = require('./a').foo;", 1, false),
            ("const { foo } = require('./a').sub;", 0, false),
            ("const { foo } = load('./a');", 0, false),
            ("const { foo } = obj.require('./a');", 0, false),
        ];
        for lang in [LangId::Javascript, LangId::Typescript, LangId::Tsx] {
            for (source, occurrence, expected) in cases {
                let tree =
                    crate::engine::parser::parse_source(source.as_bytes(), lang).expect("parse");
                let start = source
                    .match_indices("foo")
                    .nth(occurrence)
                    .unwrap_or_else(|| panic!("{source}: occurrence {occurrence}"))
                    .0;
                let point = tree_sitter::Point::new(0, start);
                let node = tree
                    .root_node()
                    .named_descendant_for_point_range(point, point)
                    .expect("node");
                assert_eq!(
                    is_commonjs_require_binding(node, source.as_bytes()),
                    expected,
                    "{lang}: {source} ({})",
                    node.kind()
                );
            }
        }
    }

    #[test]
    fn import_context_rust_use() {
        assert!(is_import_context(Some("use crate::stores::commit_store;")));
        assert!(is_import_context(Some(
            "pub use crate::stores::commit_store;"
        )));
    }

    #[test]
    fn import_context_python_from() {
        assert!(is_import_context(Some("from stores import commit_store")));
    }

    #[test]
    fn import_context_ruby_require() {
        assert!(is_import_context(Some("require 'commit_store'")));
        assert!(is_import_context(Some(
            "require_relative 'stores/commit_store'"
        )));
    }

    #[test]
    fn import_context_non_import() {
        assert!(!is_import_context(Some("const result = useCommitStore();")));
        assert!(!is_import_context(Some("useCommitStore.getState()")));
        assert!(!is_import_context(Some("fn main() {")));
        assert!(!is_import_context(None));
    }

    #[test]
    fn import_context_ts_export_declarations_are_not_reexports() {
        assert!(!is_import_context(Some(
            "export const useCommitStore = create()"
        )));
        assert!(!is_import_context(Some("export function foo() {")));
        assert!(!is_import_context(Some("export type CommitStore = {}")));
    }

    #[test]
    fn import_context_c_include() {
        assert!(is_import_context(Some("#include \"header.h\"")));
        assert!(is_import_context(Some("#include <stdio.h>")));
    }

    #[test]
    fn import_context_csharp_using() {
        assert!(is_import_context(Some("using System;")));
        assert!(is_import_context(Some("using static System.Math;")));
        assert!(!is_import_context(Some(
            "using var stream = new FileStream();"
        )));
    }

    #[test]
    fn import_context_zig_import() {
        assert!(is_import_context(Some("const std = @import(\"std\");")));
    }

    #[test]
    fn import_context_php_use() {
        assert!(is_import_context(Some("use App\\Models\\User;")));
    }

    #[test]
    fn import_context_swift_import() {
        assert!(is_import_context(Some("import Foundation")));
    }
}
