//! Angular プロジェクトの template (`.html` / inline `template: \`...\``) から
//! シンボル参照を収集する。
//!
//! Angular component の `.component.ts` で `templateUrl` で指定される `.html`
//! や `@Component({ template: \`...\` })` の inline template 内の Angular binding
//! 式 (`{{ }}`, `(event)=""`, `[prop]=""`, `*ngIf=""`, `[(ngModel)]=""`, 等) から
//! 識別子を抽出し、dead-code 判定時に「仮想的な参照」として扱う。
//!
//! TypeScript AST のみでは追跡できない Angular template 経由の参照を生存判定で
//! カバーするための補助的な実装。プロジェクト内に Angular プロジェクトの標識
//! (`angular.json` または `*.component.ts` ファイル) が見つからない場合は空集合を
//! 返し、非 Angular プロジェクトでのパフォーマンスへの影響を避ける。
//!
//! 抽出は Angular template parser を使わず、binding 構文の単純な走査で
//! identifier トークンを集める。dead-code 判定は false-positive を減らすのが
//! 目的なので、若干の過剰収集 (例: 式中のキーワード) は許容する。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::models::reference::{RefKind, SymbolReference};

/// テンプレートファイル 1 件あたりの最大サイズ (2MB)。
/// これを超える HTML/TS（生成物等）はスキップして応答性を保つ。
const MAX_TEMPLATE_FILE_SIZE: u64 = 2_097_152;

/// `dir` 配下が Angular プロジェクトと判定される場合、`.html` テンプレートおよび
/// `.ts` ファイル内の inline template (`template: \`...\``) から Angular binding
/// 式に出現する識別子を収集して返す。
///
/// Angular プロジェクトでない場合は空集合を返す。
pub fn collect_angular_template_refs(dir: &Path) -> HashSet<String> {
    if !is_angular_project(dir) {
        return HashSet::new();
    }
    let mut refs = HashSet::new();
    let mut used_element_selectors = HashSet::new();
    let mut selector_classes: HashMap<String, Vec<String>> = HashMap::new();
    let walker = ignore::WalkBuilder::new(dir).hidden(false).build();
    for entry in walker.flatten() {
        let path = entry.path();
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() || meta.len() > MAX_TEMPLATE_FILE_SIZE {
            continue;
        }
        match ext {
            "html" | "htm" => {
                if let Ok(content) = std::fs::read_to_string(path) {
                    extract_template_refs(&content, &mut refs);
                    extract_element_selectors(&content, &mut used_element_selectors);
                }
            }
            "ts" => {
                // Angular component は `.component.ts` が標準だが、別名で
                // @Component を使う場合もあるので拡張子のみで一括対応する。
                if let Ok(content) = std::fs::read_to_string(path) {
                    extract_inline_template_refs(&content, &mut refs);
                    extract_inline_template_element_selectors(
                        &content,
                        &mut used_element_selectors,
                    );
                    for (selector, class_name) in extract_component_selector_classes(&content) {
                        selector_classes
                            .entry(selector)
                            .or_default()
                            .push(class_name);
                    }
                }
            }
            _ => {}
        }
    }
    for selector in used_element_selectors {
        if let Some(classes) = selector_classes.get(&selector) {
            refs.extend(classes.iter().cloned());
        }
    }
    refs
}

/// `dir` が Angular プロジェクトとみなせるかを判定する。
///
/// - `angular.json` または `.angular-cli.json` が存在
/// - もしくは `package.json` に `@angular/core` 依存がある (cheap fast-path)
/// - もしくは `.component.ts` で終わるファイルが 1 件以上存在 (fallback)
///
/// fast-path で先に決着が付くケースは大規模 dir walk を skip できる。
fn is_angular_project(dir: &Path) -> bool {
    if dir.join("angular.json").is_file() || dir.join(".angular-cli.json").is_file() {
        return true;
    }
    // package.json の `@angular/core` 依存だけで判定できれば dir walk を skip する。
    if let Ok(pkg_json) = std::fs::read_to_string(dir.join("package.json"))
        && pkg_json.contains("@angular/core")
    {
        return true;
    }
    let walker = ignore::WalkBuilder::new(dir).hidden(false).build();
    for entry in walker.flatten() {
        if let Some(name) = entry.path().file_name().and_then(|n| n.to_str())
            && name.ends_with(".component.ts")
        {
            return true;
        }
    }
    false
}

/// HTML テンプレートから Angular binding 式内の識別子を抽出する。
fn extract_template_refs(content: &str, refs: &mut HashSet<String>) {
    extract_interpolation_refs(content, refs);
    extract_binding_refs(content, refs);
    extract_control_flow_refs(content, refs);
}

/// `<bz-popup>` のような Angular component element selector を抽出する。
/// selector→class 対応は `.ts` 側で別途引くため、ここでは tag 名だけを集める。
fn extract_element_selectors(content: &str, selectors: &mut HashSet<String>) {
    let bytes = content.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        let start = i + 1;
        if start >= bytes.len()
            || matches!(bytes[start], b'/' | b'!' | b'?' | b'@')
            || !is_html_tag_start(bytes[start])
        {
            i += 1;
            continue;
        }
        let mut j = start + 1;
        while j < bytes.len() && is_html_tag_continue(bytes[j]) {
            j += 1;
        }
        if let Ok(tag) = std::str::from_utf8(&bytes[start..j])
            && tag.contains('-')
        {
            selectors.insert(tag.to_ascii_lowercase());
        }
        i = j;
    }
}

fn is_html_tag_start(b: u8) -> bool {
    b.is_ascii_alphabetic()
}

fn is_html_tag_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')
}

/// `{{ ... }}` 補間式の中身から識別子を抽出する。
fn extract_interpolation_refs(content: &str, refs: &mut HashSet<String>) {
    let bytes = content.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            let start = i + 2;
            let mut j = start;
            while j + 1 < bytes.len() && !(bytes[j] == b'}' && bytes[j + 1] == b'}') {
                j += 1;
            }
            if j + 1 >= bytes.len() {
                break;
            }
            if let Ok(expr) = std::str::from_utf8(&bytes[start..j]) {
                extract_identifiers(expr, refs);
            }
            i = j + 2;
        } else {
            i += 1;
        }
    }
}

/// Angular の属性バインディングの右辺式から識別子を抽出する。
///
/// 対象パターン:
///   - `(event)="expr"`     event binding
///   - `[prop]="expr"`      property binding
///   - `[(ngModel)]="expr"` two-way binding
///   - `*ngIf="expr"` 等    structural directive
///   - `let-x="expr"`       template input variable
fn extract_binding_refs(content: &str, refs: &mut HashSet<String>) {
    let bytes = content.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let prev_is_boundary = i == 0 || is_attr_separator(bytes[i - 1]);
        if !prev_is_boundary {
            i += 1;
            continue;
        }
        let starts_binding = matches!(bytes[i], b'(' | b'[' | b'*')
            || (i + 4 < bytes.len() && &bytes[i..i + 4] == b"let-");
        if !starts_binding {
            i += 1;
            continue;
        }
        // 属性名を読む: `=` まで進める
        let mut j = i;
        while j < bytes.len() {
            let b = bytes[j];
            if b == b'=' || is_attr_separator(b) || b == b'>' || b == b'/' {
                break;
            }
            j += 1;
        }
        if j >= bytes.len() || bytes[j] != b'=' {
            i = j.max(i + 1);
            continue;
        }
        j += 1; // `=` の次へ
        if j >= bytes.len() {
            i = j;
            continue;
        }
        if bytes[j] == b'"' || bytes[j] == b'\'' {
            // 引用符あり: 対応する閉じ引用符まで
            let quote = bytes[j];
            j += 1;
            let start = j;
            while j < bytes.len() && bytes[j] != quote {
                j += 1;
            }
            if j >= bytes.len() {
                break;
            }
            if let Ok(expr) = std::str::from_utf8(&bytes[start..j]) {
                extract_identifiers(expr, refs);
            }
            i = j + 1;
        } else {
            // 引用符なし属性値 (`[kana]=userNameForIcon`): 空白 / `>` / `/` まで
            let start = j;
            while j < bytes.len()
                && !is_attr_separator(bytes[j])
                && bytes[j] != b'>'
                && bytes[j] != b'/'
            {
                j += 1;
            }
            if let Ok(expr) = std::str::from_utf8(&bytes[start..j]) {
                extract_identifiers(expr, refs);
            }
            i = j;
        }
    }
}

/// 属性区切り文字 (空白・タブ・改行) かを判定する。
fn is_attr_separator(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

/// 式テキストから JavaScript/TypeScript の識別子トークンを抽出する。
///
/// 先頭が英字/`_`/`$`、続きが英数/`_`/`$` を識別子として扱う。`$event` 等の
/// Angular 予約語や `true`/`false` も含まれるが、対象シンボル名が一致しない
/// 限り dead 判定への影響はない (生存判定を緩める方向にしか作用しない)。
fn extract_identifiers(expr: &str, refs: &mut HashSet<String>) {
    let bytes = expr.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if is_ident_start(b) {
            let start = i;
            i += 1;
            while i < bytes.len() && is_ident_continue(bytes[i]) {
                i += 1;
            }
            if let Ok(ident) = std::str::from_utf8(&bytes[start..i]) {
                refs.insert(ident.to_string());
            }
        } else {
            i += 1;
        }
    }
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b'$'
}

fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// TypeScript ファイル内の `@Component({ template: \`...\` })` 形式の inline
/// template から識別子を抽出する。
///
/// `template:` キーの直後の `` ` `` / `'` / `"` 文字列リテラルを抽出し、
/// その中身を HTML テンプレートと同じく走査する。
fn extract_inline_template_refs(content: &str, refs: &mut HashSet<String>) {
    let bytes = content.as_bytes();
    let needle = b"template:";
    let mut i = 0;
    while i + needle.len() <= bytes.len() {
        if &bytes[i..i + needle.len()] != needle {
            i += 1;
            continue;
        }
        if i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_') {
            i += 1;
            continue;
        }
        let mut j = i + needle.len();
        while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
            j += 1;
        }
        if j >= bytes.len() {
            break;
        }
        let quote = bytes[j];
        if quote != b'`' && quote != b'\'' && quote != b'"' {
            i = j.max(i + 1);
            continue;
        }
        j += 1;
        let start = j;
        while j < bytes.len() && bytes[j] != quote {
            if bytes[j] == b'\\' && j + 1 < bytes.len() {
                j += 2;
            } else {
                j += 1;
            }
        }
        if j >= bytes.len() {
            break;
        }
        if let Ok(template_content) = std::str::from_utf8(&bytes[start..j]) {
            extract_template_refs(template_content, refs);
        }
        i = j + 1;
    }
}

fn extract_inline_template_element_selectors(content: &str, selectors: &mut HashSet<String>) {
    for_each_inline_template(content, |template| {
        extract_element_selectors(template, selectors);
    });
}

fn for_each_inline_template(content: &str, mut f: impl FnMut(&str)) {
    let bytes = content.as_bytes();
    let needle = b"template:";
    let mut i = 0;
    while i + needle.len() <= bytes.len() {
        if &bytes[i..i + needle.len()] != needle {
            i += 1;
            continue;
        }
        if i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_') {
            i += 1;
            continue;
        }
        let mut j = i + needle.len();
        while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
            j += 1;
        }
        if j >= bytes.len() {
            break;
        }
        let quote = bytes[j];
        if quote != b'`' && quote != b'\'' && quote != b'"' {
            i = j.max(i + 1);
            continue;
        }
        j += 1;
        let start = j;
        while j < bytes.len() && bytes[j] != quote {
            if bytes[j] == b'\\' && j + 1 < bytes.len() {
                j += 2;
            } else {
                j += 1;
            }
        }
        if j >= bytes.len() {
            break;
        }
        if let Ok(template_content) = std::str::from_utf8(&bytes[start..j]) {
            f(template_content);
        }
        i = j + 1;
    }
}

fn extract_component_selector_classes(content: &str) -> Vec<(String, String)> {
    let bytes = content.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(rel) = content[i..].find("@Component") {
        let comp_start = i + rel;
        let Some(open_rel) = content[comp_start..].find('(') else {
            break;
        };
        let open = comp_start + open_rel;
        let Some(close) = find_matching_paren(bytes, open) else {
            break;
        };
        let metadata = &content[open + 1..close];
        let selectors = extract_component_selectors_from_metadata(metadata);
        if !selectors.is_empty()
            && let Some(class_name) = find_class_name_after(content, close + 1)
        {
            for selector in selectors {
                out.push((selector, class_name.clone()));
            }
        }
        i = close + 1;
    }
    out
}

fn extract_component_selectors_from_metadata(metadata: &str) -> Vec<String> {
    let mut selectors = Vec::new();
    let bytes = metadata.as_bytes();
    let needle = b"selector";
    let mut i = 0;
    while i + needle.len() <= bytes.len() {
        if &bytes[i..i + needle.len()] != needle {
            i += 1;
            continue;
        }
        let before_ok = i == 0 || !is_ident_continue(bytes[i - 1]);
        let after = i + needle.len();
        let after_ok = after >= bytes.len() || !is_ident_continue(bytes[after]);
        if !before_ok || !after_ok {
            i += 1;
            continue;
        }
        let mut j = after;
        while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t' || bytes[j] == b'\n') {
            j += 1;
        }
        if j >= bytes.len() || bytes[j] != b':' {
            i += 1;
            continue;
        }
        j += 1;
        while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t' || bytes[j] == b'\n') {
            j += 1;
        }
        if j >= bytes.len() || !matches!(bytes[j], b'\'' | b'"' | b'`') {
            i = j.max(i + 1);
            continue;
        }
        let quote = bytes[j];
        j += 1;
        let start = j;
        while j < bytes.len() && bytes[j] != quote {
            if bytes[j] == b'\\' && j + 1 < bytes.len() {
                j += 2;
            } else {
                j += 1;
            }
        }
        if j >= bytes.len() {
            break;
        }
        if let Ok(raw) = std::str::from_utf8(&bytes[start..j]) {
            selectors.extend(raw.split(',').filter_map(selector_to_element_key));
        }
        i = j + 1;
    }
    selectors
}

fn selector_to_element_key(selector: &str) -> Option<String> {
    let s = selector.trim();
    if s.is_empty() || s.starts_with('[') || s.starts_with('.') {
        return None;
    }
    let end = s
        .find(|c: char| c.is_whitespace() || matches!(c, '[' | '.' | ':'))
        .unwrap_or(s.len());
    let name = &s[..end];
    if name.contains('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        Some(name.to_ascii_lowercase())
    } else {
        None
    }
}

fn find_class_name_after(content: &str, start: usize) -> Option<String> {
    let tail = content.get(start..)?;
    let class_pos = find_keyword(tail, "class")?;
    let bytes = tail.as_bytes();
    let mut i = class_pos + "class".len();
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t' || bytes[i] == b'\n') {
        i += 1;
    }
    if i >= bytes.len() || !is_ident_start(bytes[i]) {
        return None;
    }
    let name_start = i;
    i += 1;
    while i < bytes.len() && is_ident_continue(bytes[i]) {
        i += 1;
    }
    std::str::from_utf8(&bytes[name_start..i])
        .ok()
        .map(str::to_string)
}

/// `bytes[open]` が `(` のとき、対応する閉じ `)` の index を返す。
/// 文字列リテラル (`"` / `'` / `` ` ``) とエスケープを考慮してネストを数える。
/// 対応が取れない場合は None。
fn find_matching_paren(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = open;
    let mut in_str: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = in_str {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == q {
                in_str = None;
            }
            i += 1;
            continue;
        }
        match b {
            b'"' | b'\'' | b'`' => in_str = Some(b),
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// `haystack` 内で単語境界に囲まれた `keyword` の開始バイト位置を返す。
/// `items` の中の `of` のような部分一致を弾くため識別子境界を確認する。
fn find_keyword(haystack: &str, keyword: &str) -> Option<usize> {
    let hb = haystack.as_bytes();
    let kb = keyword.as_bytes();
    if kb.is_empty() || hb.len() < kb.len() {
        return None;
    }
    let mut i = 0;
    while i + kb.len() <= hb.len() {
        if &hb[i..i + kb.len()] == kb {
            let before_ok = i == 0 || !is_ident_continue(hb[i - 1]);
            let after = i + kb.len();
            let after_ok = after >= hb.len() || !is_ident_continue(hb[after]);
            if before_ok && after_ok {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Angular 17+ 制御フローブロック (`@if` / `@else if` / `@for` / `@switch` / `@case` /
/// `@defer` / `@let`) の条件式・反復式から識別子を抽出する。
///
/// テンプレート parser は使わず、`@keyword` の後の balanced parentheses (`@let` は
/// `=` 後〜`;`) を式として切り出す簡易走査。dead-code の live 補正用途なので多少の
/// 過剰収集は許容するが、`@for` のループ変数束縛 (`item` / `let i`) は同名 component
/// member を live と誤判定して dead を取りこぼすため除外する。
fn extract_control_flow_refs(content: &str, refs: &mut HashSet<String>) {
    let bytes = content.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'@' {
            i += 1;
            continue;
        }
        // `@` の前が識別子継続文字 (メールアドレス `a@b` 等) なら制御フローではない
        if i > 0 && is_ident_continue(bytes[i - 1]) {
            i += 1;
            continue;
        }
        let kw_start = i + 1;
        let mut k = kw_start;
        while k < bytes.len() && bytes[k].is_ascii_alphabetic() {
            k += 1;
        }
        let keyword = &bytes[kw_start..k];
        // keyword 後の空白を読み飛ばす
        let mut p = k;
        while p < bytes.len() && is_attr_separator(bytes[p]) {
            p += 1;
        }
        match keyword {
            b"if" | b"switch" | b"case" | b"defer" => {
                if p < bytes.len()
                    && bytes[p] == b'('
                    && let Some(end) = find_matching_paren(bytes, p)
                {
                    if let Ok(expr) = std::str::from_utf8(&bytes[p + 1..end]) {
                        extract_identifiers(expr, refs);
                    }
                    i = end + 1;
                    continue;
                }
                i = k.max(i + 1);
            }
            b"for" => {
                if p < bytes.len()
                    && bytes[p] == b'('
                    && let Some(end) = find_matching_paren(bytes, p)
                {
                    if let Ok(expr) = std::str::from_utf8(&bytes[p + 1..end]) {
                        extract_for_block_refs(expr, refs);
                    }
                    i = end + 1;
                    continue;
                }
                i = k.max(i + 1);
            }
            b"else" => {
                // `@else if (cond)` のみ式を持つ (`@else {` は式なし)
                if bytes[p..].starts_with(b"if") {
                    let mut q = p + 2;
                    while q < bytes.len() && is_attr_separator(bytes[q]) {
                        q += 1;
                    }
                    if q < bytes.len()
                        && bytes[q] == b'('
                        && let Some(end) = find_matching_paren(bytes, q)
                    {
                        if let Ok(expr) = std::str::from_utf8(&bytes[q + 1..end]) {
                            extract_identifiers(expr, refs);
                        }
                        i = end + 1;
                        continue;
                    }
                }
                i = k.max(i + 1);
            }
            b"let" => {
                // `@let name = expr;` の `=` 後〜`;` を式とする (name はローカル束縛)
                let mut q = p;
                while q < bytes.len() && bytes[q] != b'=' && bytes[q] != b';' && bytes[q] != b'\n' {
                    q += 1;
                }
                if q < bytes.len() && bytes[q] == b'=' {
                    q += 1;
                    let start = q;
                    // RHS 終端 `;` は文字列リテラル内の `;` を無視して探す
                    // (`@let x = fn("a;b")` のような式を途中で切らない)
                    let mut in_str: Option<u8> = None;
                    while q < bytes.len() {
                        let b = bytes[q];
                        if let Some(qt) = in_str {
                            // 文字列リテラル末尾が `\` で終わると q がバッファ長を超えて
                            // 後続の slice (`&bytes[start..q]`) でパニックするため境界を確認する
                            if b == b'\\' && q + 1 < bytes.len() {
                                q += 2;
                                continue;
                            }
                            if b == qt {
                                in_str = None;
                            }
                            q += 1;
                            continue;
                        }
                        match b {
                            b'"' | b'\'' | b'`' => in_str = Some(b),
                            b';' => break,
                            _ => {}
                        }
                        q += 1;
                    }
                    if let Ok(expr) = std::str::from_utf8(&bytes[start..q]) {
                        extract_identifiers(expr, refs);
                    }
                    i = q;
                    continue;
                }
                i = k.max(i + 1);
            }
            _ => {
                i = k.max(i + 1);
            }
        }
    }
}

/// `@for (item of items; track item.id; let i = $index)` の式から、ループ変数束縛
/// (`item` / `let i`) と構文キーワードを除いた参照識別子を抽出する。ローカル束縛を
/// 拾うと同名 component member を live と誤判定して dead を取りこぼすため除外する。
fn extract_for_block_refs(expr: &str, refs: &mut HashSet<String>) {
    let mut locals: HashSet<String> = HashSet::new();
    let mut candidates: HashSet<String> = HashSet::new();
    for segment in expr.split(';') {
        let seg = segment.trim();
        if let Some(rest) = seg.strip_prefix("let ") {
            // `let i = $index` / `let i = $index, e = $even`: 左辺名はローカル束縛
            for binding in rest.split(',') {
                if let Some(lhs) = binding.split('=').next() {
                    extract_identifiers(lhs, &mut locals);
                }
            }
            continue;
        }
        if let Some(of_idx) = find_keyword(seg, "of") {
            // `item of items`: `of` の左 (ループ変数) はローカル束縛として除外集合へ。
            // 右の iterable 式は `item.children` のように同名 component member を含み得る
            // ため、locals フィルタを通さず直接参照として扱う (codex 指摘)。
            extract_identifiers(&seg[..of_idx], &mut locals);
            extract_identifiers(&seg[of_idx + 2..], refs);
            continue;
        }
        // track 等のセグメント: keyword を除いて識別子抽出
        let cleaned = seg.strip_prefix("track ").unwrap_or(seg);
        extract_identifiers(cleaned, &mut candidates);
    }
    for id in candidates {
        if !locals.contains(&id) {
            refs.insert(id);
        }
    }
}

// ============================================================================
// 位置付き参照検索 (refs コマンド用)
//
// dead-code 用の `collect_angular_template_refs` (名前集合・位置なし・過剰収集許容)
// とは別に、`refs` コマンド向けに Angular template の binding 式に出現するシンボル
// 参照を path/line/col/context 付きで返す。codex 設計相談 (GitLab #18) の方針:
//   - 外部 `.html` は `@Component` の `templateUrl` で component に紐付くものだけ走査
//     (紐付け不能な html は skip し、common method 名のノイズを避ける)
//   - inline template (`@Component({ template: `...` })`) も対象
//   - binding の左辺 (event/property 名) は対象外 (式右辺のみ抽出)
//   - pipe 名 (`| date`)、`$event` 等の Angular local、`*ngFor` の `let`/`as`/`of`
//     束縛変数、JS 予約語/リテラルは除外
// dead-code 経路 (上の collect_*) には一切手を入れず、純粋に追加実装する。
// ============================================================================

/// component に紐付く template の集約結果。
struct ComponentTemplates {
    /// `templateUrl` で component に紐付く外部 html の絶対パス (重複除去済み、dir 内)。
    linked_html: Vec<PathBuf>,
    /// inline template (component `.ts` 内)。
    inline: Vec<InlineTemplate>,
}

/// `.ts` 内の inline template 1 件。
struct InlineTemplate {
    /// inline template を含む `.ts` の絶対パス。
    ts_path: PathBuf,
    /// template リテラルの中身。
    content: String,
    /// `.ts` ファイル内での template 中身先頭の (row, col) (0-indexed, byte col)。
    base_row: usize,
    base_col: usize,
}

/// Angular template から `symbol_name` の参照を位置付きで検索する。
///
/// 非 Angular プロジェクト、または該当なしの場合は空 Vec。パスは `dir` 基準の
/// 絶対パスで返し、呼び出し側 (`find_references`) の `relativize_paths` で相対化される。
pub fn find_angular_template_references(
    symbol_name: &str,
    dir: &Path,
    glob: Option<&str>,
) -> Vec<SymbolReference> {
    let names = [symbol_name.to_string()];
    find_angular_template_references_batch(&names, dir, glob)
        .into_iter()
        .next()
        .unwrap_or_default()
}

/// batch 版。複数シンボルを 1 回の template 走査で検索する (`refs --names` 用)。
/// 戻り値は `symbol_names` と同じ並び・同じ長さの Vec。
pub fn find_angular_template_references_batch(
    symbol_names: &[String],
    dir: &Path,
    glob: Option<&str>,
) -> Vec<Vec<SymbolReference>> {
    if symbol_names.is_empty() {
        return Vec::new();
    }
    let Some(ctx) = AngularBatchContext::prepare(dir, glob) else {
        return vec![Vec::new(); symbol_names.len()];
    };
    find_angular_template_references_batch_with_context(symbol_names, &ctx)
}

/// 事前に組み立てた [`AngularBatchContext`] を再利用してバッチ参照検索を行う。
///
/// chunk 分割された大規模 batch (`refs --names`) では、chunk 毎に
/// `find_angular_template_references_batch` を呼ぶと `is_angular_project` の全ディレクトリ
/// 走査と `collect_component_templates` の全 `.ts` parse が chunk 数倍走る。コンテキストを
/// 一度だけ組み立てて使い回すことで chunk 数 → 1 回に減らす。
pub fn find_angular_template_references_batch_with_context(
    symbol_names: &[String],
    ctx: &AngularBatchContext,
) -> Vec<Vec<SymbolReference>> {
    let mut out: Vec<Vec<SymbolReference>> = vec![Vec::new(); symbol_names.len()];
    if symbol_names.is_empty() {
        return out;
    }
    // Angular template の identifier は case-sensitive。名前 → 出力 index。
    let mut name_to_ix: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, n) in symbol_names.iter().enumerate() {
        if !n.is_empty() {
            name_to_ix.entry(n.as_str()).or_default().push(i);
        }
    }
    if name_to_ix.is_empty() {
        return out;
    }

    // inline template: component `.ts` 内に式があるので、ts 座標へ変換して emit。
    for inl in &ctx.model.inline {
        let Ok(ts_content) = std::fs::read_to_string(&inl.ts_path) else {
            continue;
        };
        let path_str = inl.ts_path.to_string_lossy().to_string();
        scan_template_region_emit(
            &inl.content,
            inl.base_row,
            inl.base_col,
            &ts_content,
            &path_str,
            &name_to_ix,
            &mut out,
        );
    }

    // 外部 html: templateUrl で component に紐付くものだけ走査。
    for html_path in &ctx.model.linked_html {
        let Ok(content) = std::fs::read_to_string(html_path) else {
            continue;
        };
        let path_str = html_path.to_string_lossy().to_string();
        scan_template_region_emit(&content, 0, 0, &content, &path_str, &name_to_ix, &mut out);
    }

    for refs in &mut out {
        refs.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.line.cmp(&b.line)));
    }
    out
}

/// chunk 横断で再利用するための前処理結果。
/// 非 Angular リポ (`prepare` が `None`) の場合は呼び出し側で template 走査を完全に skip する。
pub struct AngularBatchContext {
    model: ComponentTemplates,
}

impl AngularBatchContext {
    /// `dir` が Angular プロジェクトでない場合は `None` を返す (chunk 横断で再判定不要)。
    pub fn prepare(dir: &Path, glob: Option<&str>) -> Option<Self> {
        // dir を canonicalize し、以降の templateUrl 解決・境界チェックの基準にする。
        let dir_canon = std::fs::canonicalize(dir).ok()?;
        let dir = dir_canon.as_path();
        if !is_angular_project(dir) {
            return None;
        }
        // glob は実ファイルパスで判定する (`src/**/*.html` のようなパス限定 glob にも対応)。
        let glob_ov = build_glob_override(dir, glob);
        let model = collect_component_templates(dir, &glob_ov);
        Some(Self { model })
    }
}

/// `glob` から override matcher を構築する。`glob` 未指定 / 構築失敗時は `None` (= 全許可)。
fn build_glob_override(dir: &Path, glob: Option<&str>) -> Option<ignore::overrides::Override> {
    let g = glob?;
    let mut ob = ignore::overrides::OverrideBuilder::new(dir);
    ob.add(g).ok()?;
    ob.build().ok()
}

/// `path` が glob にマッチするか。`ov` が `None` (glob 未指定) なら常に true。
/// 既存 `refs --glob` と揃え、whitelist に明示マッチしたパスのみ許可する。
fn path_matches_glob(ov: &Option<ignore::overrides::Override>, path: &Path) -> bool {
    match ov {
        None => true,
        Some(o) => o.matched(path, false).is_whitelist(),
    }
}

/// `dir` 配下の `.ts` を走査し、`@Component` の `templateUrl` / inline `template` を集める。
///
/// glob 指定時は、外部 html / inline を含む `.ts` の実パスが glob にマッチするものだけを残す
/// (templateUrl の発見自体には `.ts` の読み込みが必要なので、`.ts` の走査自体は glob で
/// 絞らない)。
fn collect_component_templates(
    dir: &Path,
    glob_ov: &Option<ignore::overrides::Override>,
) -> ComponentTemplates {
    let mut linked_html: Vec<PathBuf> = Vec::new();
    let mut seen_html: HashSet<PathBuf> = HashSet::new();
    let mut inline: Vec<InlineTemplate> = Vec::new();

    // refs の collect_files と可視性ポリシーを揃える (hidden を除外、.gitignore を尊重)。
    // 通常の refs が拾わない `.hidden/` 配下のテンプレートから参照が出ないようにする。
    let walker = ignore::WalkBuilder::new(dir)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .build();
    for entry in walker.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("ts") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() || meta.len() > MAX_TEMPLATE_FILE_SIZE {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        // component 紐付けの担保: `@Component` を含む .ts のみを対象にする。
        if !content.contains("@Component") {
            continue;
        }
        // 外部 html: 解決後の実パスが glob にマッチするものだけ採用。
        for url in extract_template_urls(&content) {
            if let Some(p) = resolve_template_path(path, &url, dir)
                && path_matches_glob(glob_ov, &p)
                && seen_html.insert(p.clone())
            {
                linked_html.push(p);
            }
        }
        // inline template: その `.ts` 自体が glob にマッチするときだけ採用。
        if path_matches_glob(glob_ov, path) {
            for (tpl, base_row, base_col) in extract_inline_templates_with_pos(&content) {
                inline.push(InlineTemplate {
                    ts_path: path.to_path_buf(),
                    content: tpl,
                    base_row,
                    base_col,
                });
            }
        }
    }
    ComponentTemplates {
        linked_html,
        inline,
    }
}

/// `templateUrl: '...'` の文字列値をすべて返す。
fn extract_template_urls(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = content.as_bytes();
    let needle = b"templateUrl";
    let mut i = 0;
    while i + needle.len() <= bytes.len() {
        if &bytes[i..i + needle.len()] != needle {
            i += 1;
            continue;
        }
        // 前が識別子継続文字なら別シンボル (`myTemplateUrl` 等)。
        if i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_') {
            i += 1;
            continue;
        }
        let mut j = i + needle.len();
        while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
            j += 1;
        }
        if j >= bytes.len() || bytes[j] != b':' {
            i += needle.len();
            continue;
        }
        j += 1;
        while j < bytes.len()
            && (bytes[j] == b' ' || bytes[j] == b'\t' || bytes[j] == b'\n' || bytes[j] == b'\r')
        {
            j += 1;
        }
        if j >= bytes.len() {
            break;
        }
        let quote = bytes[j];
        if quote != b'\'' && quote != b'"' && quote != b'`' {
            i = j;
            continue;
        }
        j += 1;
        let start = j;
        while j < bytes.len() && bytes[j] != quote {
            j += 1;
        }
        if j >= bytes.len() {
            break;
        }
        if let Ok(url) = std::str::from_utf8(&bytes[start..j]) {
            out.push(url.to_string());
        }
        i = j + 1;
    }
    out
}

/// inline `template:` の中身を (content, base_row, base_col) で返す。
/// base_row/base_col は `.ts` ファイル内での template 中身先頭位置 (0-indexed, byte col)。
fn extract_inline_templates_with_pos(content: &str) -> Vec<(String, usize, usize)> {
    let mut out = Vec::new();
    let bytes = content.as_bytes();
    let needle = b"template:";
    let mut i = 0;
    while i + needle.len() <= bytes.len() {
        if &bytes[i..i + needle.len()] != needle {
            i += 1;
            continue;
        }
        // 前が識別子継続文字なら別キー (`templateUrl:` は別途処理済みなので除外)。
        if i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_') {
            i += 1;
            continue;
        }
        let mut j = i + needle.len();
        while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
            j += 1;
        }
        if j >= bytes.len() {
            break;
        }
        let quote = bytes[j];
        if quote != b'`' && quote != b'\'' && quote != b'"' {
            i = j.max(i + 1);
            continue;
        }
        j += 1;
        let start = j;
        while j < bytes.len() && bytes[j] != quote {
            if bytes[j] == b'\\' && j + 1 < bytes.len() {
                j += 2;
            } else {
                j += 1;
            }
        }
        if j >= bytes.len() {
            break;
        }
        if let Ok(tpl) = std::str::from_utf8(&bytes[start..j]) {
            let (base_row, base_col) = byte_to_row_col(content, start);
            out.push((tpl.to_string(), base_row, base_col));
        }
        i = j + 1;
    }
    out
}

/// `templateUrl` を `.ts` の親ディレクトリ基準で解決し、canonicalize 後に `dir`
/// (canonical) 配下の実ファイルなら返す。
///
/// canonicalize で symlink を解決するため、`foo.component.html -> /etc/secret` のような
/// symlink を `templateUrl` にしても workspace 外のファイルを読み出さない (fail-closed)。
/// `dir` は呼び出し側で canonicalize 済みであることが前提。
fn resolve_template_path(ts_path: &Path, url: &str, dir: &Path) -> Option<PathBuf> {
    // 絶対 URL や `http(s):` は対象外。
    if url.is_empty() || url.starts_with("http://") || url.starts_with("https://") {
        return None;
    }
    let parent = ts_path.parent()?;
    let joined = parent.join(url);
    // canonicalize は symlink を解決し、存在しないパスでは Err になる。
    let canon = std::fs::canonicalize(&joined).ok()?;
    if !canon.starts_with(dir) {
        return None;
    }
    // 通常ファイルかつサイズ上限内のみ採用 (`.ts` 側と同じく巨大 HTML をスキップして応答性を保つ)。
    let meta = std::fs::metadata(&canon).ok()?;
    if !meta.is_file() || meta.len() > MAX_TEMPLATE_FILE_SIZE {
        return None;
    }
    Some(canon)
}

/// template 領域 (`region`) を走査し、`name_to_ix` に一致する参照を `out` に emit する。
///
/// `base_row` / `base_col` は `region` の先頭が属するファイル上の位置 (inline template
/// なら `.ts` 内、外部 html なら 0,0)。`file_content` は context 行抽出用のファイル全体。
fn scan_template_region_emit(
    region: &str,
    base_row: usize,
    base_col: usize,
    file_content: &str,
    file_path: &str,
    name_to_ix: &HashMap<&str, Vec<usize>>,
    out: &mut [Vec<SymbolReference>],
) {
    // Pass 1: template 全体の `*ngFor` / `let-` / `as` ローカル束縛名を集める。
    // ループ変数 (`let item of items` の `item`) は別の式 (`{{ item.name }}`) でも
    // 使われ得るため、式単位ではなく region 全体でローカル集合を作って除外する。
    let mut region_locals: HashSet<String> = HashSet::new();
    for_each_template_expr(region, &mut |expr, _base| {
        region_locals.extend(template_expr_locals(expr).into_iter().map(str::to_string));
    });

    // Pass 2: 各式から component member 参照を emit する。
    let mut sink = |ident: &str, byte_off: usize| {
        let Some(ixs) = name_to_ix.get(ident) else {
            return;
        };
        let (trow, tcol) = byte_to_row_col(region, byte_off);
        let frow = base_row + trow;
        let fcol = if trow == 0 { base_col + tcol } else { tcol };
        let ctx = file_line_context(file_content, frow);
        for &ix in ixs {
            out[ix].push(SymbolReference {
                path: file_path.to_string(),
                line: frow,
                column: fcol,
                context: Some(ctx.clone()),
                kind: Some(RefKind::Reference),
                confidence: None,
            });
        }
    };
    for_each_template_expr(region, &mut |expr, base| {
        for_each_expr_member_ref(expr, base, &region_locals, &mut sink);
    });
}

/// template 内のすべての binding 式 (`{{ }}` 補間 + 属性 binding 右辺) を
/// `(expr, content 内 byte 位置)` で `f` に渡す。
fn for_each_template_expr(content: &str, f: &mut impl FnMut(&str, usize)) {
    each_interpolation_expr(content, f);
    each_binding_expr(content, f);
}

/// `{{ expr }}` 補間式の中身を `(expr, byte 位置)` で `f` に渡す。
fn each_interpolation_expr(content: &str, f: &mut impl FnMut(&str, usize)) {
    let bytes = content.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            let start = i + 2;
            let mut j = start;
            while j + 1 < bytes.len() && !(bytes[j] == b'}' && bytes[j + 1] == b'}') {
                j += 1;
            }
            if j + 1 >= bytes.len() {
                break;
            }
            if let Ok(expr) = std::str::from_utf8(&bytes[start..j]) {
                f(expr, start);
            }
            i = j + 2;
        } else {
            i += 1;
        }
    }
}

/// `(event)="expr"` / `[prop]="expr"` / `*ngIf="expr"` 等の binding 右辺式を
/// `(expr, byte 位置)` で `f` に渡す。binding 名 (左辺) は対象外。
fn each_binding_expr(content: &str, f: &mut impl FnMut(&str, usize)) {
    let bytes = content.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let prev_is_boundary = i == 0 || is_attr_separator(bytes[i - 1]);
        if !prev_is_boundary {
            i += 1;
            continue;
        }
        let starts_binding = matches!(bytes[i], b'(' | b'[' | b'*')
            || (i + 4 < bytes.len() && &bytes[i..i + 4] == b"let-");
        if !starts_binding {
            i += 1;
            continue;
        }
        // 属性名を読み飛ばす: `=` まで進める。
        let mut j = i;
        while j < bytes.len() {
            let b = bytes[j];
            if b == b'=' || is_attr_separator(b) || b == b'>' || b == b'/' {
                break;
            }
            j += 1;
        }
        if j >= bytes.len() || bytes[j] != b'=' {
            i = j.max(i + 1);
            continue;
        }
        j += 1; // `=` の次へ
        if j >= bytes.len() {
            i = j;
            continue;
        }
        if bytes[j] == b'"' || bytes[j] == b'\'' {
            let quote = bytes[j];
            j += 1;
            let start = j;
            while j < bytes.len() && bytes[j] != quote {
                j += 1;
            }
            if j >= bytes.len() {
                break;
            }
            if let Ok(expr) = std::str::from_utf8(&bytes[start..j]) {
                f(expr, start);
            }
            i = j + 1;
        } else {
            // 引用符なし属性値: 空白 / `>` / `/` まで
            let start = j;
            while j < bytes.len()
                && !is_attr_separator(bytes[j])
                && bytes[j] != b'>'
                && bytes[j] != b'/'
            {
                j += 1;
            }
            if let Ok(expr) = std::str::from_utf8(&bytes[start..j]) {
                f(expr, start);
            }
            i = j;
        }
    }
}

/// 式 `expr` (file 内 byte 位置 `byte_base` から始まる) の component member 参照候補を
/// `(ident, abs_byte_offset)` で sink に渡す。
///
/// 除外: 文字列リテラル (`'...'` / `"..."` / `` `...` ``) 内、`$`-prefix (`$event` 等)、
/// pipe 名 (`| date` の `date`)、property access の右辺 (`a.b` の `b`)、`locals`
/// (template 全体の `let`/`as`/`of`/`in` 束縛変数)、JS 予約語/リテラル。
fn for_each_expr_member_ref(
    expr: &str,
    byte_base: usize,
    locals: &HashSet<String>,
    sink: &mut impl FnMut(&str, usize),
) {
    let bytes = expr.as_bytes();
    let mut i = 0;
    // 直前の significant byte 種別: 0=なし, b'.'=member access, b'|'=pipe, その他。
    let mut prev_sig: u8 = 0;
    // 文字列リテラル内かどうか (開始引用符)。文字列内の識別子は参照に数えない
    // (`toast('save')` の `save`、`{'display': ...}` の `display` 等を誤検出しない)。
    let mut in_str: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = in_str {
            if b == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if b == q {
                in_str = None;
            }
            prev_sig = b'"'; // 文字列の後は member access でも pipe でもない
            i += 1;
            continue;
        }
        if b == b'\'' || b == b'"' || b == b'`' {
            in_str = Some(b);
            prev_sig = b'"';
            i += 1;
            continue;
        }
        if b == b'|' {
            // `||` (logical or) と単一 `|` (pipe) を区別する。
            if i + 1 < bytes.len() && bytes[i + 1] == b'|' {
                prev_sig = b'O';
                i += 2;
            } else {
                prev_sig = b'|';
                i += 1;
            }
            continue;
        }
        if b == b'?' && i + 1 < bytes.len() && bytes[i + 1] == b'.' {
            // optional chaining `a?.b`: `b` は member access。
            prev_sig = b'.';
            i += 2;
            continue;
        }
        if is_ident_start(b) {
            let start = i;
            i += 1;
            while i < bytes.len() && is_ident_continue(bytes[i]) {
                i += 1;
            }
            let ident = &expr[start..i];
            let skip = ident.starts_with('$')
                || prev_sig == b'|'
                || prev_sig == b'.'
                || locals.contains(ident)
                || is_template_reserved(ident);
            if !skip {
                sink(ident, byte_base + start);
            }
            prev_sig = b'a'; // identifier (member access でも pipe でもない)
            continue;
        }
        if !b.is_ascii_whitespace() {
            prev_sig = b;
        }
        i += 1;
    }
}

/// 式内で `let X` / `as Y` / `X of/in` で束縛されるローカル変数名を集める
/// (`*ngFor="let item of items"` の `item` 等。component member 参照と誤認しないため)。
fn template_expr_locals(expr: &str) -> HashSet<&str> {
    let mut locals = HashSet::new();
    // ident トークン列を keyword 認識付きで取り出す。
    let mut toks: Vec<&str> = Vec::new();
    let bytes = expr.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if is_ident_start(bytes[i]) {
            let start = i;
            i += 1;
            while i < bytes.len() && is_ident_continue(bytes[i]) {
                i += 1;
            }
            toks.push(&expr[start..i]);
        } else {
            i += 1;
        }
    }
    for k in 0..toks.len() {
        match toks[k] {
            "let" | "as" => {
                if let Some(name) = toks.get(k + 1) {
                    locals.insert(*name);
                }
            }
            "of" | "in" if k > 0 => {
                locals.insert(toks[k - 1]);
            }
            _ => {}
        }
    }
    locals
}

/// template 式で component member 参照とみなさない JS 予約語/リテラル/microsyntax keyword。
fn is_template_reserved(ident: &str) -> bool {
    matches!(
        ident,
        "true"
            | "false"
            | "null"
            | "undefined"
            | "this"
            | "new"
            | "typeof"
            | "instanceof"
            | "void"
            | "in"
            | "of"
            | "as"
            | "let"
    )
}

/// `content` の byte offset を (row, col) (0-indexed, byte col) に変換する。
fn byte_to_row_col(content: &str, byte: usize) -> (usize, usize) {
    let bytes = content.as_bytes();
    let limit = byte.min(bytes.len());
    let mut row = 0;
    let mut line_start = 0;
    for (idx, &b) in bytes.iter().enumerate().take(limit) {
        if b == b'\n' {
            row += 1;
            line_start = idx + 1;
        }
    }
    (row, limit - line_start)
}

/// `content` の `row` 行目を context として返す (前後空白を trim、256 byte 上限・UTF-8 境界安全)。
fn file_line_context(content: &str, row: usize) -> String {
    let line = content.lines().nth(row).unwrap_or("").trim();
    if line.len() <= 256 {
        return line.to_string();
    }
    let mut end = 256;
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    line[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolation_extracts_identifier() {
        let html = r#"<span>{{ greeting() }}</span>"#;
        let mut refs = HashSet::new();
        extract_template_refs(html, &mut refs);
        assert!(refs.contains("greeting"));
    }

    /// `@let` の文字列リテラルが末尾バックスラッシュで終わっても slice 範囲外パニックしない
    #[test]
    fn let_unterminated_string_escape_no_panic() {
        let mut refs = HashSet::new();
        // RHS 文字列が末尾 `\` で終わる (エスケープ未完) 入力。
        // 修正前は q がバッファ長を超え `&bytes[start..q]` でパニックしていた。
        extract_control_flow_refs("@let x = \"\\", &mut refs);
        // パニックしないことが目的 (refs 内容は問わない)
        let _ = refs;
    }

    #[test]
    fn event_binding_extracts_handler() {
        let html = r#"<input (ngModelChange)="onCellCheckChanged()">"#;
        let mut refs = HashSet::new();
        extract_template_refs(html, &mut refs);
        assert!(refs.contains("onCellCheckChanged"));
    }

    #[test]
    fn property_binding_extracts_method_call() {
        let html = r#"<label [ngStyle]="{'display': isHeaderDisabled() ? 'none' : ''}"></label>"#;
        let mut refs = HashSet::new();
        extract_template_refs(html, &mut refs);
        assert!(refs.contains("isHeaderDisabled"));
    }

    #[test]
    fn structural_directive_extracts_identifier() {
        let html = r#"<div *ngIf="canShow()"></div>"#;
        let mut refs = HashSet::new();
        extract_template_refs(html, &mut refs);
        assert!(refs.contains("canShow"));
    }

    #[test]
    fn two_way_binding_extracts_identifier() {
        let html = r#"<input [(ngModel)]="headerCheck">"#;
        let mut refs = HashSet::new();
        extract_template_refs(html, &mut refs);
        assert!(refs.contains("headerCheck"));
    }

    #[test]
    fn event_binding_with_argument_extracts_handler() {
        let html =
            r#"<custom (valueChanged)="propValueChanged($event, 'RingNoAnswerSec')"></custom>"#;
        let mut refs = HashSet::new();
        extract_template_refs(html, &mut refs);
        assert!(refs.contains("propValueChanged"));
    }

    #[test]
    fn ngfor_let_binding_extracts_source_expression() {
        let html = r#"<li *ngFor="let item of items"></li>"#;
        let mut refs = HashSet::new();
        extract_template_refs(html, &mut refs);
        assert!(refs.contains("items"));
    }

    #[test]
    fn plain_text_does_not_collect_random_words() {
        let html = r#"<div>hello world</div>"#;
        let mut refs = HashSet::new();
        extract_template_refs(html, &mut refs);
        // バインディング外のテキストノードは収集対象外
        assert!(!refs.contains("hello"));
        assert!(!refs.contains("world"));
    }

    #[test]
    fn inline_template_extracts_identifiers() {
        let ts = r#"
@Component({
    selector: 'app-sample',
    template: `<button (click)="onClick()">{{ label }}</button>`,
})
export class SampleComponent {}
"#;
        let mut refs = HashSet::new();
        extract_inline_template_refs(ts, &mut refs);
        assert!(refs.contains("onClick"));
        assert!(refs.contains("label"));
    }

    #[test]
    fn inline_template_with_single_quotes_extracts_identifiers() {
        let ts = r#"
@Component({ template: '<span (click)="run()"></span>' })
export class SampleComponent {}
"#;
        let mut refs = HashSet::new();
        extract_inline_template_refs(ts, &mut refs);
        assert!(refs.contains("run"));
    }

    #[test]
    fn inline_template_marker_inside_identifier_is_ignored() {
        // `myTemplate:` のような偽属性はマッチさせない
        let ts = r#"const myTemplate: string = `(click)="ghost()"`;"#;
        let mut refs = HashSet::new();
        extract_inline_template_refs(ts, &mut refs);
        assert!(!refs.contains("ghost"));
    }

    #[test]
    fn is_angular_project_detects_angular_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("angular.json"), "{}").unwrap();
        assert!(is_angular_project(dir.path()));
    }

    #[test]
    fn is_angular_project_detects_component_ts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("sample.component.ts"),
            "@Component({}) class C {}",
        )
        .unwrap();
        assert!(is_angular_project(dir.path()));
    }

    #[test]
    fn is_angular_project_returns_false_without_markers() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.ts"), "export const x = 1;").unwrap();
        assert!(!is_angular_project(dir.path()));
    }

    /// package.json の `@angular/core` 依存だけで Angular プロジェクトと判定し、
    /// .component.ts を探す dir walk を skip する (cheap fast-path)。
    #[test]
    fn is_angular_project_detects_via_package_json_dependency() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"dependencies":{"@angular/core":"^18.0.0"}}"#,
        )
        .unwrap();
        assert!(is_angular_project(dir.path()));
    }

    /// `@angular/core` を含まない package.json があるリポジトリでは fast-path を素通りし、
    /// `.component.ts` の有無で判定する (fallback)。
    #[test]
    fn is_angular_project_package_json_without_angular_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"dependencies":{"react":"^18.0.0"}}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("Foo.tsx"), "export const Foo = () => 1;").unwrap();
        // .component.ts も無いので false (fallback walk が空)
        assert!(!is_angular_project(dir.path()));
    }

    #[test]
    fn collect_returns_empty_for_non_angular_project() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("page.html"), r#"<input (click)="ghost()">"#).unwrap();
        assert!(collect_angular_template_refs(dir.path()).is_empty());
    }

    #[test]
    fn collect_finds_refs_in_angular_project() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("sample.component.ts"),
            r#"
@Component({
    selector: 'app-sample',
    templateUrl: './sample.component.html',
})
export class SampleComponent {
    public headerCheckChanged(): void {}
    public isHeaderDisabled(): boolean { return false; }
}
"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("sample.component.html"),
            r#"<label [ngStyle]="{'display': isHeaderDisabled() ? 'none' : ''}">
  <input type="checkbox" [(ngModel)]="headerCheck"
         (ngModelChange)="headerCheckChanged()">
</label>"#,
        )
        .unwrap();
        let refs = collect_angular_template_refs(dir.path());
        assert!(refs.contains("headerCheckChanged"));
        assert!(refs.contains("isHeaderDisabled"));
    }

    #[test]
    fn collect_skips_html_outside_angular_project() {
        // Angular マーカーがないプロジェクトでは HTML を走査しない
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("index.html"),
            r#"<span>{{ shouldNotMatch }}</span>"#,
        )
        .unwrap();
        let refs = collect_angular_template_refs(dir.path());
        assert!(!refs.contains("shouldNotMatch"));
    }

    #[test]
    fn unquoted_property_binding_extracts_member() {
        // `[kana]=userNameForIcon` のように引用符なしの属性値も拾う (GitLab #14)
        let html = r#"<bz-default-icon [kana]=userNameForIcon></bz-default-icon>"#;
        let mut refs = HashSet::new();
        extract_template_refs(html, &mut refs);
        assert!(refs.contains("userNameForIcon"));
    }

    #[test]
    fn unquoted_binding_stops_at_tag_end() {
        // 引用符なし属性値はタグ終端 `>` で止まり、後続テキストを巻き込まない
        let html = r#"<div [hidden]=isCollapsed>body text</div>"#;
        let mut refs = HashSet::new();
        extract_template_refs(html, &mut refs);
        assert!(refs.contains("isCollapsed"));
        assert!(!refs.contains("body"));
        assert!(!refs.contains("text"));
    }

    #[test]
    fn control_flow_if_extracts_predicate() {
        // Angular 17+ `@if (predicate())` の述語を拾う (GitLab #14)
        let html = "@if (isHeaderFeedbackVisible()) {\n  <div>x</div>\n}";
        let mut refs = HashSet::new();
        extract_template_refs(html, &mut refs);
        assert!(refs.contains("isHeaderFeedbackVisible"));
    }

    #[test]
    fn control_flow_else_if_extracts_predicate() {
        let html = "@if (a()) {} @else if (otherPredicate()) {}";
        let mut refs = HashSet::new();
        extract_template_refs(html, &mut refs);
        assert!(refs.contains("otherPredicate"));
    }

    #[test]
    fn control_flow_for_extracts_iterable_not_loop_var() {
        // `@for (item of items; track item.id)`: iterable は拾うが @for 宣言部の
        // ループ変数束縛 item は拾わない。補間 `{{ item.x }}` 内の item はスコープ
        // 追跡外で過剰収集を許容するため、ここでは制御フロー宣言部の抽出のみ検証する。
        let html = "@for (item of menuItems; track item.id) { <li></li> }";
        let mut refs = HashSet::new();
        extract_control_flow_refs(html, &mut refs);
        assert!(refs.contains("menuItems"));
        assert!(
            !refs.contains("item"),
            "@for 宣言部のループ変数 item はローカル束縛なので拾わない"
        );
    }

    #[test]
    fn control_flow_switch_case_extracts_expressions() {
        let html = "@switch (currentMode()) {\n  @case (computedCase()) { <a></a> }\n}";
        let mut refs = HashSet::new();
        extract_template_refs(html, &mut refs);
        assert!(refs.contains("currentMode"));
        assert!(refs.contains("computedCase"));
    }

    #[test]
    fn control_flow_let_extracts_rhs() {
        // `@let total = computeTotal();` の右辺を拾う
        let html = "@let total = computeTotal();";
        let mut refs = HashSet::new();
        extract_template_refs(html, &mut refs);
        assert!(refs.contains("computeTotal"));
    }

    #[test]
    fn email_at_sign_not_treated_as_control_flow() {
        // メールアドレスの `@` を制御フロー keyword と誤認しない (前が識別子継続文字)
        let html = r#"<a href="mailto:user@ifExample.com">contact</a>"#;
        let mut refs = HashSet::new();
        extract_control_flow_refs(html, &mut refs);
        assert!(!refs.contains("Example"));
        assert!(!refs.contains("ifExample"));
    }

    #[test]
    fn collect_finds_unquoted_and_control_flow_refs_in_angular_project() {
        // GitLab #14 の統合再現: 引用符なし binding + @if 制御フロー
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("header.component.ts"),
            r#"
@Component({ templateUrl: './header.component.html' })
export class HeaderComponent {
    get userNameForIcon(): string { return ''; }
    protected isHeaderFeedbackVisible(): boolean { return false; }
}
"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("header.component.html"),
            "<bz-default-icon [kana]=userNameForIcon></bz-default-icon>\n@if (isHeaderFeedbackVisible()) { <div>x</div> }",
        )
        .unwrap();
        let refs = collect_angular_template_refs(dir.path());
        assert!(refs.contains("userNameForIcon"));
        assert!(refs.contains("isHeaderFeedbackVisible"));
    }

    #[test]
    fn control_flow_for_of_rhs_member_not_filtered_by_loop_var() {
        // `of` 右辺の `item.children` は同名ループ変数 item があっても member 参照として拾う
        // (codex 指摘: of 右辺を locals フィルタ対象外にする)
        let html = "@for (item of item.children; track item.id) { <a></a> }";
        let mut refs = HashSet::new();
        extract_control_flow_refs(html, &mut refs);
        assert!(
            refs.contains("item"),
            "of 右辺の item は component member 参照として拾う"
        );
        assert!(refs.contains("children"));
    }

    #[test]
    fn control_flow_let_rhs_ignores_string_semicolon() {
        // `@let` RHS の文字列リテラル内 `;` で式を切らない (codex 指摘)
        let html = r#"@let label = formatPair("a;b", computeTotal());"#;
        let mut refs = HashSet::new();
        extract_control_flow_refs(html, &mut refs);
        assert!(refs.contains("computeTotal"));
        assert!(refs.contains("formatPair"));
    }

    // ---- 位置付き参照検索 (refs コマンド用) ----

    fn html_ref_lines(refs: &[SymbolReference], suffix: &str) -> Vec<usize> {
        refs.iter()
            .filter(|r| r.path.ends_with(suffix))
            .map(|r| r.line)
            .collect()
    }

    #[test]
    fn template_refs_finds_external_html_event_binding() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("foo.component.ts"),
            "@Component({ templateUrl: './foo.component.html' })\nexport class FooComponent { showModal(): void {} }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("foo.component.html"),
            "<button (click)=\"showModal()\"></button>",
        )
        .unwrap();
        let refs = find_angular_template_references("showModal", dir.path(), None);
        assert_eq!(
            html_ref_lines(&refs, "foo.component.html"),
            vec![0],
            "{refs:?}"
        );
        assert!(refs.iter().all(|r| r.kind == Some(RefKind::Reference)));
    }

    #[test]
    fn template_refs_finds_inline_template_with_ts_coordinates() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("foo.component.ts"),
            "import { Component } from '@angular/core';\n@Component({\n  template: `<b (click)=\"doSave()\"></b>`,\n})\nexport class FooComponent { doSave(): void {} }\n",
        )
        .unwrap();
        let refs = find_angular_template_references("doSave", dir.path(), None);
        // inline の参照は .ts の template 行 (0-indexed 行2) を指す。
        assert_eq!(
            html_ref_lines(&refs, "foo.component.ts"),
            vec![2],
            "{refs:?}"
        );
    }

    #[test]
    fn template_refs_excludes_binding_name_left_side() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.component.ts"),
            "@Component({ templateUrl: './a.component.html' })\nexport class A {}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("a.component.html"),
            "<x (myEvent)=\"handler()\" [myProp]=\"value\"></x>",
        )
        .unwrap();
        // event/property 名 (左辺) は参照に含めない。
        assert!(find_angular_template_references("myEvent", dir.path(), None).is_empty());
        assert!(find_angular_template_references("myProp", dir.path(), None).is_empty());
    }

    #[test]
    fn template_refs_excludes_dollar_locals_and_pipes_and_member_access() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.component.ts"),
            "@Component({ templateUrl: './a.component.html' })\nexport class A {}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("a.component.html"),
            "<x (e)=\"sel($event)\">{{ total | date }}{{ user.date }}</x>",
        )
        .unwrap();
        // $event (Angular local)、pipe 名 date、member access (user.date の date) は除外。
        assert!(find_angular_template_references("event", dir.path(), None).is_empty());
        assert!(find_angular_template_references("date", dir.path(), None).is_empty());
        // 一方で式中の関数呼び出しは拾う。
        assert!(!find_angular_template_references("sel", dir.path(), None).is_empty());
    }

    #[test]
    fn template_refs_excludes_ngfor_loop_local_across_expressions() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.component.ts"),
            "@Component({ templateUrl: './a.component.html' })\nexport class A {}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("a.component.html"),
            "<li *ngFor=\"let item of items\">{{ item.label }}</li>",
        )
        .unwrap();
        // ループ変数 item は別式 {{ item.label }} でも参照とみなさない。
        assert!(find_angular_template_references("item", dir.path(), None).is_empty());
        // iterable の items は component member 参照として拾う。
        assert!(!find_angular_template_references("items", dir.path(), None).is_empty());
    }

    #[test]
    fn template_refs_skips_unlinked_html() {
        let dir = tempfile::tempdir().unwrap();
        // component はあるが、orphan.html を templateUrl で参照していない。
        std::fs::write(
            dir.path().join("a.component.ts"),
            "@Component({ templateUrl: './a.component.html' })\nexport class A { doIt(): void {} }\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("a.component.html"), "<b></b>").unwrap();
        std::fs::write(
            dir.path().join("orphan.html"),
            "<button (click)=\"doIt()\"></button>",
        )
        .unwrap();
        let refs = find_angular_template_references("doIt", dir.path(), None);
        assert!(
            html_ref_lines(&refs, "orphan.html").is_empty(),
            "紐付け不能な html は走査しない: {refs:?}"
        );
    }

    #[test]
    fn template_refs_links_shared_template_via_concrete_component() {
        // GitLab #18 の本質: メソッドは abstract base に定義され、共有 html は
        // concrete component の templateUrl で紐付く。
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("base.component.ts"),
            "@Directive()\nexport abstract class Base { protected showFavoriteEditModal(): void {} }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("agent.component.ts"),
            "@Component({ templateUrl: './layout.html' })\nexport class Agent extends Base {}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("layout.html"),
            "<x (editModalOpenClicked)=\"showFavoriteEditModal()\"></x>",
        )
        .unwrap();
        let refs = find_angular_template_references("showFavoriteEditModal", dir.path(), None);
        assert_eq!(html_ref_lines(&refs, "layout.html"), vec![0], "{refs:?}");
    }

    #[test]
    fn template_refs_empty_for_non_angular_project() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.ts"), "export function doIt() {}\n").unwrap();
        std::fs::write(
            dir.path().join("b.html"),
            "<button (click)=\"doIt()\"></button>",
        )
        .unwrap();
        assert!(find_angular_template_references("doIt", dir.path(), None).is_empty());
    }

    #[test]
    fn template_refs_batch_aligns_results() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.component.ts"),
            "@Component({ templateUrl: './a.component.html' })\nexport class A {}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("a.component.html"),
            "<x (a)=\"foo()\" (b)=\"bar()\"></x>",
        )
        .unwrap();
        let names = vec!["foo".to_string(), "missing".to_string(), "bar".to_string()];
        let batch = find_angular_template_references_batch(&names, dir.path(), None);
        assert_eq!(batch.len(), 3);
        assert_eq!(batch[0].len(), 1, "foo");
        assert!(batch[1].is_empty(), "missing");
        assert_eq!(batch[2].len(), 1, "bar");
    }

    #[test]
    fn template_refs_glob_scopes_html_vs_inline() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.component.ts"),
            "@Component({ templateUrl: './a.component.html', template: `<i (x)=\"inlineFn()\"></i>` })\nexport class A {}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("a.component.html"),
            "<x (y)=\"htmlFn()\"></x>",
        )
        .unwrap();
        // --glob '**/*.html' → 外部 html のみ
        assert!(
            !find_angular_template_references("htmlFn", dir.path(), Some("**/*.html")).is_empty()
        );
        assert!(
            find_angular_template_references("inlineFn", dir.path(), Some("**/*.html")).is_empty()
        );
        // --glob '**/*.ts' → inline のみ
        assert!(find_angular_template_references("htmlFn", dir.path(), Some("**/*.ts")).is_empty());
        assert!(
            !find_angular_template_references("inlineFn", dir.path(), Some("**/*.ts")).is_empty()
        );
    }

    #[test]
    fn template_refs_excludes_string_literals() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.component.ts"),
            "@Component({ templateUrl: './a.component.html' })\nexport class A {}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("a.component.html"),
            "<x (click)=\"toast('save')\" [ngStyle]=\"{'display': flag ? 'a' : 'b'}\"></x>",
        )
        .unwrap();
        // 文字列リテラル内の `save` / quoted key `'display'` は参照に数えない。
        assert!(find_angular_template_references("save", dir.path(), None).is_empty());
        assert!(find_angular_template_references("display", dir.path(), None).is_empty());
        // 一方で関数呼び出し / 式中の変数は拾う。
        assert!(!find_angular_template_references("toast", dir.path(), None).is_empty());
        assert!(!find_angular_template_references("flag", dir.path(), None).is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn template_refs_rejects_symlink_template_url_escape() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            outside.path(),
            "<button (click)=\"outsideSecret()\"></button>",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("foo.component.ts"),
            "@Component({ templateUrl: './foo.component.html' })\nexport class Foo { outsideSecret(): void {} }\n",
        )
        .unwrap();
        // foo.component.html を workspace 外の実ファイルへの symlink にする。
        std::os::unix::fs::symlink(outside.path(), dir.path().join("foo.component.html")).unwrap();
        let refs = find_angular_template_references("outsideSecret", dir.path(), None);
        // symlink 経由で workspace 外を読まない (html 参照は 0)。
        assert!(
            refs.iter().all(|r| !r.path.ends_with(".html")),
            "symlink escape を許してはいけない: {refs:?}"
        );
    }

    #[test]
    fn template_refs_respects_path_specific_glob() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/foo.component.ts"),
            "@Component({ templateUrl: './foo.component.html' })\nexport class Foo { doIt(): void {} }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/foo.component.html"),
            "<button (click)=\"doIt()\"></button>",
        )
        .unwrap();
        // パス限定 glob でも実パスで判定して html 参照を拾う。
        for glob in ["src/**/*.html", "**/*.component.html", "**/*.html"] {
            let refs = find_angular_template_references("doIt", dir.path(), Some(glob));
            assert_eq!(
                html_ref_lines(&refs, "foo.component.html"),
                vec![0],
                "glob {glob} で html 参照が取れること"
            );
        }
    }

    #[test]
    fn template_refs_skips_oversized_external_html() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("big.component.ts"),
            "@Component({ templateUrl: './big.component.html' })\nexport class Big { bigFn(): void {} }\n",
        )
        .unwrap();
        // MAX_TEMPLATE_FILE_SIZE 超の html はスキップする (.ts 側と同じ応答性ポリシー)。
        let padding = "x".repeat(MAX_TEMPLATE_FILE_SIZE as usize);
        std::fs::write(
            dir.path().join("big.component.html"),
            format!("<button (click)=\"bigFn()\"></button>\n<!-- {padding} -->"),
        )
        .unwrap();
        assert!(find_angular_template_references("bigFn", dir.path(), None).is_empty());
    }

    #[test]
    fn template_refs_skips_hidden_dir_like_collect_files() {
        let dir = tempfile::tempdir().unwrap();
        // 通常の component (可視) と hidden 配下の component を用意する。
        std::fs::write(
            dir.path().join("ok.component.ts"),
            "@Component({ templateUrl: './ok.component.html' })\nexport class Ok { okFn(): void {} }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("ok.component.html"),
            "<button (click)=\"okFn()\"></button>",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join(".hidden")).unwrap();
        std::fs::write(
            dir.path().join(".hidden/sneaky.component.ts"),
            "@Component({ templateUrl: './sneaky.component.html' })\nexport class Sneaky { hiddenFn(): void {} }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join(".hidden/sneaky.component.html"),
            "<button (click)=\"hiddenFn()\"></button>",
        )
        .unwrap();
        // 可視 component の参照は拾うが、hidden 配下のテンプレートからは拾わない
        // (refs の collect_files が hidden を除外するのと揃える)。
        assert!(!find_angular_template_references("okFn", dir.path(), None).is_empty());
        assert!(find_angular_template_references("hiddenFn", dir.path(), None).is_empty());
    }
}
