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

use std::collections::HashSet;
use std::path::Path;

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
                }
            }
            "ts" => {
                // Angular component は `.component.ts` が標準だが、別名で
                // @Component を使う場合もあるので拡張子のみで一括対応する。
                if let Ok(content) = std::fs::read_to_string(path) {
                    extract_inline_template_refs(&content, &mut refs);
                }
            }
            _ => {}
        }
    }
    refs
}

/// `dir` が Angular プロジェクトとみなせるかを判定する。
///
/// - `angular.json` または `.angular-cli.json` が存在
/// - もしくは `.component.ts` で終わるファイルが 1 件以上存在
fn is_angular_project(dir: &Path) -> bool {
    if dir.join("angular.json").is_file() || dir.join(".angular-cli.json").is_file() {
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
}
