//! impact streaming Pass で使う純粋なフィルタ関数群。
//!
//! ここにまとめる理由:
//!   - どれも `&str` を受け取って `bool` を返すだけの純粋関数で、state を持たない。
//!   - Pass 2 / Pass 3 の両方から呼ばれる共通ユーティリティ。
//!   - テストを同居させておくと import 文の言語別バリエーションを追加しやすい。

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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn import_context_ts_export_without_from() {
        assert!(!is_import_context(Some(
            "export const useCommitStore = create()"
        )));
        assert!(!is_import_context(Some("export function foo() {")));
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
