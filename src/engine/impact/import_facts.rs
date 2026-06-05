//! TS/TSX/JS 系 impact 経路の名前衝突 false positive 抑制 helper。
//!
//! - `extract_ts_import_basenames`: ref file の `import ... from "..."` / `require(...)` の
//!   specifier 末尾 basename (extension drop) 集合を tree-sitter で抽出する
//! - `ref_file_directly_imports_source`: ref file の specifier basename と source file の
//!   basename を heuristic で照合し、直接 import かを判定する
//! - `is_ts_local_shadow_context`: ref の context 行を見て、対象 identifier がローカル
//!   binding (interface / type / const / let / var / function / class / destructured params)
//!   で shadow されているかを軽量 heuristic で判定する
//!
//! 完全な TS resolver (tsconfig paths / barrel re-export / export * 等) は別 Issue
//! (`2026-06-06-astro-sight-impact-import-graph-resolver.md`) に切り出し、ここでは fail-closed
//! な軽量 heuristic のみ実装する。parse/read 失敗時は `None` を返し、呼び出し側で
//! 「直接 import あり」「shadow なし」相当として high impact を維持する (false negative 回避)。

use std::collections::HashSet;

use lru::LruCache;

use crate::engine::parser;
use crate::language::LangId;

/// ref file から抽出した「直接 import している specifier 末尾 basename 集合」と言語情報。
/// 1 ファイル分の事実を per-worker LRU cache に保持する。
#[derive(Debug, Clone)]
pub(super) struct RefFileFacts {
    pub(super) lang_id: LangId,
    pub(super) import_basenames: HashSet<String>,
}

/// `source` を tree-sitter で parse して `import_statement` の source 文字列 (specifier) の
/// 末尾 basename (extension drop) 集合を返す。失敗時は空集合 (= fail-closed 側)。
///
/// 対応構文:
/// - `import X from "specifier";`
/// - `import { X } from "specifier";`
/// - `import "specifier";`
/// - `export { X } from "specifier";`
/// - `export * from "specifier";`
/// - `const X = require("specifier");` / `require("specifier")`
pub(super) fn extract_ts_import_basenames(source: &[u8], lang_id: LangId) -> HashSet<String> {
    let mut out = HashSet::new();
    if !is_ts_family(lang_id) {
        return out;
    }
    let Ok(tree) = parser::parse_source(source, lang_id) else {
        return out;
    };
    walk_for_import_specifiers(tree.root_node(), source, &mut out);
    out
}

fn is_ts_family(lang_id: LangId) -> bool {
    matches!(
        lang_id,
        LangId::Typescript | LangId::Tsx | LangId::Javascript
    )
}

fn walk_for_import_specifiers(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    out: &mut HashSet<String>,
) {
    let kind = node.kind();
    match kind {
        "import_statement" | "export_statement" => {
            // tree-sitter-typescript / -javascript では import/export の source は
            // string_literal で表現される。子から探す (field 名は grammar により異なる)。
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if matches!(child.kind(), "string" | "string_literal")
                    && let Some(text) = string_node_inner_text(child, source)
                    && let Some(bn) = specifier_basename(&text)
                {
                    out.insert(bn);
                }
            }
        }
        "call_expression" => {
            // `require("specifier")` (CommonJS)
            let mut cursor = node.walk();
            let children: Vec<_> = node.named_children(&mut cursor).collect();
            if let Some(first) = children.first()
                && first.kind() == "identifier"
                && first.utf8_text(source).map(str::trim) == Ok("require")
            {
                for c in &children[1..] {
                    if c.kind() == "arguments" {
                        let mut ac = c.walk();
                        for arg in c.named_children(&mut ac) {
                            if (arg.kind() == "string" || arg.kind() == "string_literal")
                                && let Some(text) = string_node_inner_text(arg, source)
                                && let Some(bn) = specifier_basename(&text)
                            {
                                out.insert(bn);
                            }
                        }
                    }
                }
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_for_import_specifiers(child, source, out);
    }
}

/// tree-sitter の string ノードから quote を除いた中身を取得する。
fn string_node_inner_text(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let text = node.utf8_text(source).ok()?.trim();
    let bytes = text.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' || first == b'\'' || first == b'`') && first == last {
            return Some(text[1..text.len() - 1].to_string());
        }
    }
    // string_fragment の場合は子ノードの text を取り直す必要があるが、
    // tree-sitter は string 全体を返すので上の trim_quote で大体カバーできる
    Some(text.to_string())
}

/// specifier (`@/lib/db/schema` / `./db/schema` / `lib/db/schema.ts` 等) から末尾 basename
/// (extension drop、ディレクトリの最後のセグメント) を返す。`index` / `mod` は親ディレクトリ名
/// に置換する (barrel index は親の名前で参照されることが多い)。
fn specifier_basename(specifier: &str) -> Option<String> {
    let trimmed = specifier.trim();
    if trimmed.is_empty() {
        return None;
    }
    // 末尾 / は除去
    let s = trimmed.trim_end_matches('/');
    let last = s.rsplit('/').next()?;
    let stem = match last.rsplit_once('.') {
        Some((before, "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs")) => before,
        _ => last,
    };
    if stem.is_empty() {
        return None;
    }
    // `index` / `mod` は親ディレクトリ名にフォールバック
    if matches!(stem, "index" | "mod") {
        let without_last = s.rsplit_once('/').map(|(p, _)| p)?;
        let parent_last = without_last.rsplit('/').next()?;
        return Some(parent_last.to_string());
    }
    Some(stem.to_string())
}

/// `source_path` の basename (ファイル名 stem、拡張子除去、`index`/`mod` は親ディレクトリ名)。
fn path_basename(source_path: &str) -> Option<String> {
    let p = std::path::Path::new(source_path);
    let stem = p.file_stem().and_then(|s| s.to_str())?;
    if matches!(stem, "index" | "mod") {
        let parent = p.parent()?;
        let last = parent.file_name().and_then(|s| s.to_str())?;
        return Some(last.to_string());
    }
    Some(stem.to_string())
}

/// `ref_path` (TS/TSX/JS 想定) が `source_path` を直接 import しているかを basename 照合で判定する。
/// facts が cache 未取得なら ref_path を parse して basename 集合を構築し cache に格納する。
/// 判定不能 (read / parse 失敗、source_path から basename を取れない) なら true を返し
/// 「直接 import あり」と保守的に扱う = high impact 維持 (false negative 回避)。
pub(super) fn ref_file_directly_imports_source(
    dir: &str,
    ref_path: &str,
    source_path: &str,
    facts_cache: &mut LruCache<String, Option<RefFileFacts>>,
) -> bool {
    let Some(source_basename) = path_basename(source_path) else {
        return true;
    };
    // cache lookup or build
    let facts_opt = match facts_cache.get(ref_path) {
        Some(v) => v.clone(),
        None => {
            let built = build_ref_file_facts(dir, ref_path);
            facts_cache.put(ref_path.to_string(), built.clone());
            built
        }
    };
    let Some(facts) = facts_opt else {
        return true; // parse/read 失敗時は fail-closed
    };
    if !is_ts_family(facts.lang_id) {
        return true;
    }
    facts.import_basenames.contains(&source_basename)
}

fn build_ref_file_facts(dir: &str, ref_path: &str) -> Option<RefFileFacts> {
    let full = std::path::Path::new(dir).join(ref_path);
    let utf8 = camino::Utf8Path::from_path(&full)?;
    let lang_id = LangId::from_path(utf8).ok()?;
    if !is_ts_family(lang_id) {
        return None;
    }
    let source = parser::read_file(utf8).ok()?;
    let import_basenames = extract_ts_import_basenames(&source, lang_id);
    Some(RefFileFacts {
        lang_id,
        import_basenames,
    })
}

/// ref の context 行から、対象 identifier がローカル binding (`interface` / `type` /
/// `const` / `let` / `var` / `function` / `class` / destructured params) で shadow されて
/// いるかを軽量 heuristic で判定する。TS/TSX/JS のみ。
///
/// **fail-closed**: 判定不能 (context が空、パターン不一致) なら false を返し、shadow なしと
/// 扱う = high impact 維持 (false negative 回避)。
pub(super) fn is_ts_local_shadow_context(context: &str, symbol_name: &str) -> bool {
    let ctx = context.trim();
    if ctx.is_empty() || symbol_name.is_empty() {
        return false;
    }
    // 単純なパターンマッチで網羅:
    // - `interface X`、`interface X<`、`interface X {`
    // - `type X =`、`type X<...> =`
    // - `const X` / `let X` / `var X` (右辺は問わない)
    // - `function X` / `function* X` / `async function X`
    // - `class X` / `class X<`、`class X extends`
    // - destructured params 等の `({ X })` / `{ X }: ` / `{ X, ... }`
    let patterns = [
        format!("interface {symbol_name}"),
        format!("type {symbol_name}"),
        format!("const {symbol_name}"),
        format!("let {symbol_name}"),
        format!("var {symbol_name}"),
        format!("function {symbol_name}"),
        format!("function* {symbol_name}"),
        format!("class {symbol_name}"),
    ];
    for p in &patterns {
        if let Some(pos) = ctx.find(p) {
            // パターンの後ろが identifier 継続文字でないこと (prefix match を避ける)
            let after_pos = pos + p.len();
            if let Some(next_char) = ctx[after_pos..].chars().next()
                && (next_char.is_alphanumeric() || next_char == '_' || next_char == '$')
            {
                continue;
            }
            return true;
        }
    }
    // destructured params heuristic: `{ symbol_name` または `{ symbol_name,` が `:` の前後
    // (関数引数 / 分割代入 binding)
    if has_destructured_binding(ctx, symbol_name) {
        return true;
    }
    false
}

fn has_destructured_binding(ctx: &str, name: &str) -> bool {
    // `{ name }` / `{ name,` / `{ name:` / `{ ..., name }` 等
    // brace を含み、name の前は `{` または `,` または space (区切り)、後は `,` / `}` / `:` / ` ` / `=`
    let mut i = 0;
    let bytes = ctx.as_bytes();
    while i < bytes.len() {
        if bytes[i] != b'{' {
            i += 1;
            continue;
        }
        // brace 内をスキャン
        let start = i + 1;
        let mut end = start;
        let mut depth = 1;
        while end < bytes.len() && depth > 0 {
            match bytes[end] {
                b'{' => depth += 1,
                b'}' => depth -= 1,
                _ => {}
            }
            if depth == 0 {
                break;
            }
            end += 1;
        }
        if end > bytes.len() {
            break;
        }
        let inner = &ctx[start..end.min(ctx.len())];
        if identifier_appears_in_destructure(inner, name) {
            return true;
        }
        i = end + 1;
    }
    false
}

fn identifier_appears_in_destructure(inner: &str, name: &str) -> bool {
    // inner を `,` で分割し、各要素を trim、`X` / `X = ...` / `X: ...` / `...X` の形で name と一致するか
    for raw in inner.split(',') {
        let part = raw.trim().trim_start_matches("...").trim();
        if part.is_empty() {
            continue;
        }
        // `name` or `name = default` or `name: alias` or `alias: name`
        let head = part.split(['=', ':', ' ']).next().unwrap_or("").trim();
        if head == name {
            return true;
        }
        // `alias: name` (rename) の name 部分
        if let Some((_, after_colon)) = part.split_once(':') {
            let tail = after_colon.split(['=', ' ']).next().unwrap_or("").trim();
            if tail == name {
                return true;
            }
        }
    }
    false
}

/// TS/TSX/JS 同士の cross-file ref で、ref file が source module を直接 import せず、
/// TS/TSX/JS 同士なら `true` を返す。`local_shadow_hint` は present (=より高確信度) の
/// 補助シグナルとして渡すが、現在のロジックでは "import なし" を主要条件として採用する
/// (Issue 2026-06-05-multi-attachment-conversations-fp の本質: 名前ベース cross-file refs を
/// 重い blocking 信号にしない)。barrel re-export / tsconfig paths 等の経路は別 Issue
/// (`2026-06-06-astro-sight-impact-import-graph-resolver.md`) で本格 resolver を実装する。
pub(super) fn should_route_ts_importless_ref_low(
    ref_lang_is_ts_family: bool,
    source_lang_is_ts_family: bool,
    has_direct_source_import: bool,
    _local_shadow_hint: bool,
) -> bool {
    if !ref_lang_is_ts_family || !source_lang_is_ts_family {
        return false; // TS family 以外は従来通り
    }
    // 直接 import なしなら low routing。barrel re-export 経路は別 Issue で対応する。
    !has_direct_source_import
}

/// `LangId` が TS family (TypeScript / TSX / JavaScript) かを返す。pub(super) で公開して
/// collector からも利用する。
pub(super) fn lang_is_ts_family(lang_id: LangId) -> bool {
    is_ts_family(lang_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specifier_basename_strips_extension() {
        assert_eq!(specifier_basename("@/lib/db/schema"), Some("schema".into()));
        assert_eq!(specifier_basename("./db/schema.ts"), Some("schema".into()));
        assert_eq!(specifier_basename("../schema.tsx"), Some("schema".into()));
        assert_eq!(specifier_basename("@/lib/db/index"), Some("db".into()));
        assert_eq!(specifier_basename("react"), Some("react".into()));
    }

    #[test]
    fn path_basename_handles_index() {
        assert_eq!(path_basename("lib/db/schema.ts"), Some("schema".into()));
        assert_eq!(path_basename("lib/db/index.ts"), Some("db".into()));
        assert_eq!(path_basename("schema.tsx"), Some("schema".into()));
    }

    #[test]
    fn is_ts_local_shadow_context_detects_common_patterns() {
        assert!(is_ts_local_shadow_context(
            "interface Conversation {",
            "Conversation"
        ));
        assert!(is_ts_local_shadow_context("type X = string", "X"));
        assert!(is_ts_local_shadow_context(
            "const conversations = [];",
            "conversations"
        ));
        assert!(is_ts_local_shadow_context("let foo = 1;", "foo"));
        assert!(is_ts_local_shadow_context("function bar() {", "bar"));
        assert!(is_ts_local_shadow_context(
            "class Baz extends Parent {",
            "Baz"
        ));
    }

    #[test]
    fn is_ts_local_shadow_context_detects_destructured_params() {
        assert!(is_ts_local_shadow_context(
            "function ConversationList({ conversations }: Props) {",
            "conversations"
        ));
        assert!(is_ts_local_shadow_context(
            "const { conversations, onSelect } = props;",
            "conversations"
        ));
        assert!(is_ts_local_shadow_context(
            "({ conversations = [] }: ConversationListProps) => {",
            "conversations"
        ));
    }

    #[test]
    fn is_ts_local_shadow_context_avoids_prefix_match() {
        assert!(!is_ts_local_shadow_context(
            "const conversationsList = [];",
            "conversations"
        ));
        assert!(!is_ts_local_shadow_context(
            "interface Conversations {",
            "Conversation"
        ));
    }

    #[test]
    fn extract_ts_import_basenames_picks_up_imports() {
        let src = br#"
import { conversations } from "@/lib/db/schema";
import * as utils from "./util/helpers.ts";
const ssr = require("./server/render");
export { foo } from "./bar/baz";
"#;
        let bases = extract_ts_import_basenames(src, LangId::Typescript);
        assert!(bases.contains("schema"));
        assert!(bases.contains("helpers"));
        assert!(bases.contains("render"));
        assert!(bases.contains("baz"));
    }

    #[test]
    fn should_route_ts_importless_ref_low_routes_when_no_direct_import() {
        // 直接 import なしなら shadow hint 有無に関わらず low routing
        assert!(should_route_ts_importless_ref_low(true, true, false, true));
        assert!(should_route_ts_importless_ref_low(true, true, false, false));
        // 直接 import あり → high impact 維持
        assert!(!should_route_ts_importless_ref_low(true, true, true, true));
        assert!(!should_route_ts_importless_ref_low(true, true, true, false));
        // 非 TS family → 従来通り (false)
        assert!(!should_route_ts_importless_ref_low(
            false, true, false, true
        ));
        assert!(!should_route_ts_importless_ref_low(
            true, false, false, true
        ));
    }
}
