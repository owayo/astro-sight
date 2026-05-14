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
        j += 1;
        if j >= bytes.len() || (bytes[j] != b'"' && bytes[j] != b'\'') {
            i = j;
            continue;
        }
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
}
