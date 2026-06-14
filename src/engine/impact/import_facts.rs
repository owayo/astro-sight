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
///
/// TS family は `import_basenames`、Rust は `rust_referenced_names` / `rust_has_glob_use` を使う。
/// 言語ごとに片側のフィールドだけ埋まる (もう一方は空)。
#[derive(Debug, Clone)]
pub(super) struct RefFileFacts {
    pub(super) lang_id: LangId,
    pub(super) import_basenames: HashSet<String>,
    /// Rust: `use` 経由で束縛された名前 + コード中の qualified path (`X::name`) の末尾
    /// セグメント名集合。bare identifier 参照が「別モジュールの同名関数への参照」と言える
    /// 証拠 (import / qualified path) を持つかの判定に使う。
    pub(super) rust_referenced_names: HashSet<String>,
    /// Rust: `use ...::*` (glob import) が 1 つでもあるか。glob は名前解決不能なので
    /// 証拠ありとして high 側に倒す (fail-closed)。
    pub(super) rust_has_glob_use: bool,
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

/// Rust の ref file を parse し、bare identifier 参照が「別モジュールの同名シンボルへの参照」と
/// 言える証拠 (use 経由の束縛名 / qualified path の末尾セグメント) の集合と glob import の有無を返す。
///
/// over-collection 側 (= 名前を多めに拾い high 維持) に倒す:
/// - `scoped_identifier` / `scoped_type_identifier` の `name` field (qualified path / use 経由の末尾名)
/// - `use_list` 直下の `identifier` / `type_identifier` (`use a::{json, html}` の各要素)
/// - `use_as_clause` の alias (`use a::json as j` → `j`)
///
/// 失敗時 (parse 不可) は空集合 + glob=false を返す。呼び出し側が証拠なしと扱うと low 振り分けに
/// なるが、build 自体の失敗は `build_ref_file_facts` が `None` を返して high 維持にするため、
/// ここに来る時点では parse 済み。
pub(super) fn extract_rust_ref_facts(source: &[u8]) -> (HashSet<String>, bool) {
    let mut names = HashSet::new();
    let mut has_glob = false;
    // 1) qualified path (`render::json`) の末尾セグメントを raw byte 走査で拾う。tree-sitter は
    //    マクロ引数を `token_tree` (生トークン) にするため `println!("{}", render::json(x))` 内の
    //    `::json` は AST では辿りにくい。`::` 直後の identifier を集めることでマクロ内も含め
    //    全ての qualified path 末尾名を拾う (comment/string 内も拾うが、over-collection は
    //    high 維持側で fail-closed)。
    collect_qualified_path_names(source, &mut names);
    // 2) use list (`use a::{json, html}`) の要素・alias と glob (`use a::*`) は AST で拾う。
    if let Ok(tree) = parser::parse_source(source, LangId::Rust) {
        walk_for_rust_evidence(tree.root_node(), source, &mut names, &mut has_glob);
    }
    (names, has_glob)
}

/// raw source を走査し、`::` 直後 (空白を挟む可能性も考慮) の ASCII identifier を `names` に集める。
/// `render::json` → `json`、`crate::render::json` → `render` / `json`。`::*` (glob) や `::{` (list) は
/// identifier が続かないため対象外 (それぞれ AST 側で処理)。
fn collect_qualified_path_names(source: &[u8], names: &mut HashSet<String>) {
    let n = source.len();
    let mut i = 0;
    while i + 1 < n {
        if source[i] == b':' && source[i + 1] == b':' {
            let mut j = i + 2;
            while j < n && (source[j] == b' ' || source[j] == b'\t') {
                j += 1;
            }
            let start = j;
            while j < n && (source[j].is_ascii_alphanumeric() || source[j] == b'_') {
                j += 1;
            }
            if j > start
                && let Ok(s) = std::str::from_utf8(&source[start..j])
            {
                names.insert(s.to_string());
            }
            i = (i + 2).max(j);
        } else {
            i += 1;
        }
    }
}

fn walk_for_rust_evidence(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    names: &mut HashSet<String>,
    has_glob: &mut bool,
) {
    match node.kind() {
        // qualified path (`render::json`) / use 経由 (`crate::render::json`) の末尾セグメント名。
        "scoped_identifier" | "scoped_type_identifier" => {
            if let Some(name_node) = node.child_by_field_name("name")
                && let Ok(text) = name_node.utf8_text(source)
            {
                names.insert(text.to_string());
            }
        }
        // glob import (`use crate::render::*;`) は名前解決不能 → 証拠ありとして high 側に倒す。
        "use_wildcard" => {
            *has_glob = true;
        }
        // `use a::{json, html}` の list 要素 (識別子 / alias)。
        "use_list" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                match child.kind() {
                    "identifier" | "type_identifier" => {
                        if let Ok(text) = child.utf8_text(source) {
                            names.insert(text.to_string());
                        }
                    }
                    "use_as_clause" => {
                        if let Some(alias) = child.child_by_field_name("alias")
                            && let Ok(text) = alias.utf8_text(source)
                        {
                            names.insert(text.to_string());
                        }
                    }
                    _ => {}
                }
            }
        }
        // `use a::json as j;` (top-level alias) → 束縛名 `j`。
        "use_as_clause" => {
            if let Some(alias) = node.child_by_field_name("alias")
                && let Ok(text) = alias.utf8_text(source)
            {
                names.insert(text.to_string());
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_for_rust_evidence(child, source, names, has_glob);
    }
}

fn build_ref_file_facts(dir: &str, ref_path: &str) -> Option<RefFileFacts> {
    let full = std::path::Path::new(dir).join(ref_path);
    let utf8 = camino::Utf8Path::from_path(&full)?;
    let lang_id = LangId::from_path(utf8).ok()?;
    let is_ts = is_ts_family(lang_id);
    let is_rust = lang_id == LangId::Rust;
    if !is_ts && !is_rust {
        return None;
    }
    let source = parser::read_file(utf8).ok()?;
    let import_basenames = if is_ts {
        extract_ts_import_basenames(&source, lang_id)
    } else {
        HashSet::new()
    };
    let (rust_referenced_names, rust_has_glob_use) = if is_rust {
        extract_rust_ref_facts(&source)
    } else {
        (HashSet::new(), false)
    };
    Some(RefFileFacts {
        lang_id,
        import_basenames,
        rust_referenced_names,
        rust_has_glob_use,
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

/// Rust の ref file (= `ref_path`) が `symbol_name` を import / qualified path / glob で
/// 参照する証拠を持つかを判定する。`Some(true)` = 証拠あり (high 維持側)、`Some(false)` =
/// 証拠なし (bare identifier はローカル / 無関係 → low 候補)。`None` = 判定不能
/// (parse / read 失敗) で、呼び出し側は high 維持 (fail-closed)。
pub(super) fn rust_ref_file_has_symbol_evidence(
    dir: &str,
    ref_path: &str,
    symbol_name: &str,
    facts_cache: &mut LruCache<String, Option<RefFileFacts>>,
) -> Option<bool> {
    let facts_opt = match facts_cache.get(ref_path) {
        Some(v) => v.clone(),
        None => {
            let built = build_ref_file_facts(dir, ref_path);
            facts_cache.put(ref_path.to_string(), built.clone());
            built
        }
    };
    let facts = facts_opt?;
    if facts.lang_id != LangId::Rust {
        return None;
    }
    Some(facts.rust_has_glob_use || facts.rust_referenced_names.contains(symbol_name))
}

/// Rust cross-file ref の routing 判定。
/// - 証拠なし (import / qualified path / glob いずれもなし) → `true` (low)。B1: bare identifier は
///   別モジュールの同名シンボルへの参照ではない。
/// - 証拠あり → この ref が local binding (`let json` 等) の shadow なら `true` (low)、そうでなければ
///   `false` (high)。B2: import 済みでもローカル束縛で shadow される箇所は弱い信号。
///
/// fail-closed: `has_symbol_evidence` が `None` (判定不能) の場合は呼び出し側で high 維持する
/// (この関数は呼ばない)。
pub(super) fn should_route_rust_ref_low(
    has_symbol_evidence: bool,
    local_shadow_hint: bool,
) -> bool {
    if !has_symbol_evidence {
        return true;
    }
    local_shadow_hint
}

/// Rust の context 行で identifier がローカル束縛 (`let json` / `let mut json` / `for json in`)
/// で shadow されているかを軽量 heuristic で判定する。
///
/// **fail-closed**: 判定不能 / パターン不一致なら `false` (shadow なし = high 維持)。fn 引数や
/// struct literal field (`Foo { json: x }`) のような曖昧ケースは誤って shadow と見なすと
/// false negative になるため対象外とし、明確な `let` / `for` 束縛のみ拾う。
pub(super) fn is_rust_local_shadow_context(context: &str, symbol_name: &str) -> bool {
    let ctx = context.trim();
    if ctx.is_empty() || symbol_name.is_empty() {
        return false;
    }
    // `let json` / `let mut json` / `let ref json` / `let ref mut json` / `for json in`
    let patterns = [
        format!("let {symbol_name}"),
        format!("let mut {symbol_name}"),
        format!("let ref {symbol_name}"),
        format!("let ref mut {symbol_name}"),
        format!("for {symbol_name}"),
    ];
    for p in &patterns {
        if let Some(pos) = ctx.find(p) {
            let after = pos + p.len();
            // パターン直後が identifier 継続文字なら prefix 誤判定 (`let json_value`) なので除外。
            if let Some(next_char) = ctx[after..].chars().next()
                && (next_char.is_alphanumeric() || next_char == '_')
            {
                continue;
            }
            return true;
        }
    }
    false
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

    // ---- Rust 経路 (Issue 2026-06-13-ai-status-json-symbol-fp) ----

    #[test]
    fn extract_rust_ref_facts_collects_use_and_qualified_path_names() {
        let src = br#"
use crate::render::json;
use crate::render::{html, css};
use crate::other::value as v;
fn a() { let x = render::table(); }
"#;
        let (names, glob) = extract_rust_ref_facts(src);
        assert!(names.contains("json"), "use 経由 json");
        assert!(names.contains("html"), "use list html");
        assert!(names.contains("css"), "use list css");
        assert!(names.contains("table"), "qualified path render::table");
        assert!(names.contains("v"), "alias v");
        assert!(!glob, "glob なし");
    }

    #[test]
    fn extract_rust_ref_facts_detects_glob_use() {
        let (_, glob) = extract_rust_ref_facts(b"use crate::render::*;\n");
        assert!(glob, "glob import を検出");
    }

    #[test]
    fn collect_qualified_path_names_handles_macro_embedded_paths() {
        // tree-sitter がマクロ引数を token_tree にする `println!` 内の `render::json` も拾う。
        let mut names = HashSet::new();
        collect_qualified_path_names(b"println!(\"{}\", render::json(x));", &mut names);
        // `::` の直後 (= json) を拾う。`render` は `::` の前なので対象外 (use list / 単独 import は
        // AST 側で別途拾う)。
        assert!(names.contains("json"));
        assert!(!names.contains("render"));
    }

    #[test]
    fn collect_qualified_path_names_ignores_glob_and_list() {
        let mut names = HashSet::new();
        collect_qualified_path_names(b"use a::*; use b::{c};", &mut names);
        // `::*` と `::{` の直後は identifier ではないので拾わない
        assert!(
            !names.contains("c"),
            "list 要素は textual では拾わない (AST 側で拾う)"
        );
        assert!(names.is_empty(), "glob/list の直後 identifier なし");
    }

    #[test]
    fn extract_rust_ref_facts_local_var_only_has_no_evidence() {
        // profiles.rs 相当: render::json への use / qualified path なし、bare `let json` のみ。
        let src = br#"
use std::fs;
pub fn discover() -> i32 {
    let json = fs::read_to_string("x").unwrap_or_default();
    let parsed: i32 = json.trim().parse().unwrap_or(0);
    parsed
}
"#;
        let (names, glob) = extract_rust_ref_facts(src);
        assert!(
            !names.contains("json"),
            "json への import/qualified 証拠なし"
        );
        assert!(
            names.contains("fs"),
            "fs::read_to_string の fs は qualified"
        );
        assert!(names.contains("read_to_string"));
        assert!(!glob);
    }

    #[test]
    fn should_route_rust_ref_low_b1_no_evidence() {
        // 証拠なし → shadow hint に関わらず low (B1)
        assert!(should_route_rust_ref_low(false, false));
        assert!(should_route_rust_ref_low(false, true));
    }

    #[test]
    fn should_route_rust_ref_low_b2_evidence_with_shadow() {
        // 証拠あり + local shadow → low (B2)
        assert!(should_route_rust_ref_low(true, true));
        // 証拠あり + shadow なし → high 維持
        assert!(!should_route_rust_ref_low(true, false));
    }

    #[test]
    fn is_rust_local_shadow_context_detects_let_and_for() {
        assert!(is_rust_local_shadow_context("let json = foo();", "json"));
        assert!(is_rust_local_shadow_context(
            "    let mut json = 0;",
            "json"
        ));
        assert!(is_rust_local_shadow_context("let ref json = x;", "json"));
        assert!(is_rust_local_shadow_context("for json in items {", "json"));
        // prefix 誤判定回避
        assert!(!is_rust_local_shadow_context("let json_value = 1;", "json"));
        // qualified call / use はローカル束縛ではない
        assert!(!is_rust_local_shadow_context("render::json(x)", "json"));
        assert!(!is_rust_local_shadow_context(
            "use crate::render::json;",
            "json"
        ));
    }
}
