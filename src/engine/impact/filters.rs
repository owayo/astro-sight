//! impact streaming Pass で使うフィルタ群。
//!
//! ここにまとめる理由:
//!   - import 文判定などは `&str` を受け取って `bool` を返すだけの純粋関数で、state を持たない。
//!   - Pass 2 / Pass 3 の両方から呼ばれる共通ユーティリティ。
//!   - テストを同居させておくと import 文の言語別バリエーションを追加しやすい。
//!
//! Pass 1 の cross-file 収録判定 (`CrossFileFilterContext`) も、判定材料が per-file 定数
//! だけで副作用を持たない同種のフィルタなのでここに置く。
use std::collections::HashSet;

use crate::engine::symbols;
use crate::language::LangId;
use crate::models::impact::{AffectedSymbol, HunkInfo, SignatureChange};
use crate::models::symbol::Symbol;

use super::find_overlapping_symbol;
use super::signature::is_symbol_in_changed_lines;
use super::test_context::is_in_test_context;

/// Pass 1 の cross-file 収録判定に必要な per-file 定数をまとめた借用コンテキスト。
///
/// `should_include_for_cross_file` は affected シンボル 1 件ごとに呼ばれるが、判定材料の
/// うち変化するのは `sym` だけで、残りはすべて「その diff ファイル」に固定された値。
/// ファイルごとに 1 度だけ組み立てて affected ループから使い回す。
pub(super) struct CrossFileFilterContext<'a> {
    /// 変更ファイルから抽出した全シンボル。
    pub(super) syms: &'a [Symbol],
    pub(super) hunks: &'a [HunkInfo],
    pub(super) sig_changes: &'a [SignatureChange],
    /// diff 全体のテキスト (シンボル名が変更行に出現するかの照合に使う)。
    pub(super) diff_input: &'a str,
    /// diff の新側パス。
    pub(super) file_path: &'a str,
    pub(super) root: tree_sitter::Node<'a>,
    pub(super) source: &'a [u8],
    pub(super) lang_id: LangId,
    /// 新側の実変更行 (`+` 行、0-indexed)。
    pub(super) changed_new_lines: &'a HashSet<usize>,
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
    pub(super) fn should_include_for_cross_file(&self, sym: &AffectedSymbol) -> bool {
        // 1. impl ブロックの型名とモジュール宣言をスキップ
        // モジュール宣言（例: `pub mod tensor`）は API サーフェスを変更しない。
        // 実際の内容変更は diff 内のモジュール自身のファイルから検出される。
        if sym.kind == "type" || sym.kind == "module" {
            return false;
        }
        // hunks と重なる定義シンボルは (syms, sym.name, hunks) が不変なので 1 回だけ引く。
        // Option<&Symbol> は Copy のため以降の各判定で使い回せる (旧実装は 3 回線形スキャンしていた)。
        let overlapping = find_overlapping_symbol(self.syms, &sym.name, self.hunks);
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
            symbols::is_symbol_exported(self.root, self.source, self.lang_id, &s.range)
        }) {
            return false;
        }
        // 5. 変更行にシンボル名が出現しない場合スキップ
        if !is_symbol_in_changed_lines(self.diff_input, self.file_path, &sym.name, self.lang_id) {
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
    if !matches!(
        lang_id,
        LangId::Javascript | LangId::Typescript | LangId::Tsx
    ) {
        return false;
    }
    let start = tree_sitter::Point {
        row: range.start.line,
        column: range.start.column,
    };
    let end = tree_sitter::Point {
        row: range.end.line,
        column: range.end.column,
    };
    let Some(node) = root.descendant_for_point_range(start, end) else {
        return false;
    };
    // variable シンボルの range は variable_declarator 全体を指す。念のため祖先方向にも辿る。
    let mut cur = Some(node);
    while let Some(n) = cur {
        if n.kind() == "variable_declarator" {
            return n
                .child_by_field_name("value")
                .is_some_and(|v| v.kind() == "object");
        }
        cur = n.parent();
    }
    false
}

/// 同一ファイル判定。サフィックスマッチで偽陽性を出さないよう、完全一致 or パス区切り付き
/// （`ref_path.ends_with("/{source_path}")`）で判定する。
pub(super) fn is_same_source_file(ref_path: &str, source_path: &str) -> bool {
    ref_path == source_path || ref_path.ends_with(&format!("/{source_path}"))
}

/// 参照のコンテキスト行が import/re-export 文かどうかを判定する。
pub(super) fn is_import_context(context: Option<&str>) -> bool {
    let ctx = match context {
        Some(c) => c.trim(),
        None => return false,
    };
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
    // JS/TS: const { X } = require('...'), require('...')
    if ctx.contains("= require(") || ctx.starts_with("require(") {
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
pub(super) fn is_import_context_at(context: Option<&str>, column: usize) -> bool {
    let Some(ctx) = context else {
        return false;
    };
    let segment = statement_segment_at_column(ctx, column);
    is_import_context(Some(segment))
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

    /// 1 行目全体を hunk とみなす per-file 定数を組み立ててから判定を実行する。
    fn include_for_rust(
        source: &str,
        sym: &AffectedSymbol,
        sig_changes: &[SignatureChange],
        changed_lines: &[usize],
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
        let ctx = CrossFileFilterContext {
            syms: &syms,
            hunks: &hunks,
            sig_changes,
            diff_input: &diff_input,
            file_path: "src/lib.rs",
            root,
            source: bytes,
            lang_id: LangId::Rust,
            changed_new_lines: &changed_new_lines,
        };
        ctx.should_include_for_cross_file(sym)
    }

    fn sig_change(name: &str) -> SignatureChange {
        SignatureChange {
            name: name.to_string(),
            old_signature: "fn foo()".to_string(),
            new_signature: "fn foo(a: usize)".to_string(),
        }
    }

    // 1. impl ブロックの型名 / モジュール宣言は AST を見る前に除外される
    #[test]
    fn cross_file_filter_skips_type_and_module_kind() {
        let source = "pub fn foo(a: usize) -> usize { a }\n";
        assert!(!include_for_rust(
            source,
            &affected("Foo", "type", "modified"),
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

    // 4. 非 export (pub なし) の関数は除外
    #[test]
    fn cross_file_filter_skips_non_exported_fn() {
        let source = "fn foo(a: usize) -> usize { a }\n";
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
        let ctx = CrossFileFilterContext {
            syms: &syms,
            hunks: &hunks,
            sig_changes: &[],
            diff_input,
            file_path: "mod.ts",
            root,
            source: bytes,
            lang_id: LangId::Typescript,
            changed_new_lines: &changed_new_lines,
        };
        ctx.should_include_for_cross_file(sym)
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

    #[test]
    fn same_source_file_with_prefix() {
        assert!(is_same_source_file("other/src/main.rs", "src/main.rs"));
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

        assert!(is_import_context_at(Some(line), reexport_col));
        assert!(!is_import_context_at(Some(line), call_col));
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
