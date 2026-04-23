//! Android プロジェクトの XML リソースからシンボル参照を収集する。
//!
//! `AndroidManifest.xml` の `android:name` や layout XML の `tools:context`,
//! `android:onClick`, fragment `class` 属性、カスタムビュータグ `<com.example.View>`
//! などから識別子を抽出し、dead-code 判定時に「仮想的な参照」として扱う。
//!
//! Kotlin/Java AST のみでは追跡できない Android framework 経由の dispatch を
//! 生存判定でカバーするための補助的な実装。プロジェクト内に
//! `AndroidManifest.xml` が見つからない場合は空集合を返し、非 Android プロジェクトでの
//! パフォーマンスへの影響を避ける。

use std::collections::HashSet;
use std::path::Path;

/// XML スキャン対象 1 ファイルあたりの最大サイズ (1MB)。
/// これを超える XML（生成物や画像埋め込み等）はスキップして応答性を保つ。
const MAX_XML_FILE_SIZE: u64 = 1_048_576;

/// `dir` 配下に `AndroidManifest.xml` が存在する場合、プロジェクト内の
/// 全 XML ファイルから Android リソース参照で使われている識別子を収集して返す。
///
/// 収集される識別子は次の 2 形式:
///   - 属性値の末尾セグメント (`com.example.MainActivity` → `MainActivity`)
///   - 属性値そのもの (`.MainActivity` や `MainActivity` は先頭 `.` を除去)
///
/// `AndroidManifest.xml` が見つからない場合は空集合を返す。
pub fn collect_xml_symbol_references(dir: &Path) -> HashSet<String> {
    if !has_android_manifest(dir) {
        return HashSet::new();
    }
    let mut refs = HashSet::new();
    let walker = ignore::WalkBuilder::new(dir).hidden(false).build();
    for entry in walker.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("xml") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() || meta.len() > MAX_XML_FILE_SIZE {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(path) {
            extract_android_refs(&content, &mut refs);
        }
    }
    refs
}

/// `dir` 配下に `AndroidManifest.xml` が存在するかを判定する。
fn has_android_manifest(dir: &Path) -> bool {
    let walker = ignore::WalkBuilder::new(dir).hidden(false).build();
    for entry in walker.flatten() {
        let path = entry.path();
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n == "AndroidManifest.xml")
            .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

/// XML テキストから Android リソース参照で使われる識別子を抽出する。
fn extract_android_refs(content: &str, refs: &mut HashSet<String>) {
    // 属性ベースの参照: android:name="..." / tools:context="..." /
    // android:onClick="..." / app:layout_behavior="..." / class="..."
    const ATTRS: &[&str] = &[
        "android:name",
        "tools:context",
        "android:onClick",
        "app:layout_behavior",
        "class",
    ];
    for attr in ATTRS {
        collect_attribute_values(content, attr, refs);
    }

    // カスタムビュータグ: `<com.example.MyView ...>` のようなドットを含むタグ名
    collect_custom_view_tags(content, refs);
}

/// `content` から `<attr>=\"value\"` または `<attr>=\'value\'` のパターンを抽出し、
/// 値から識別子を集めて `refs` に挿入する。
fn collect_attribute_values(content: &str, attr: &str, refs: &mut HashSet<String>) {
    let bytes = content.as_bytes();
    let needle = attr.as_bytes();
    let mut i = 0;
    while i + needle.len() < bytes.len() {
        if &bytes[i..i + needle.len()] != needle {
            i += 1;
            continue;
        }
        // 直前がトークン境界 (非 identifier 文字) であることを確認
        if i > 0 && is_attr_name_char(bytes[i - 1]) {
            i += 1;
            continue;
        }
        // attr の直後に `=` と引用符が続くかチェック（間の空白/改行は許容）
        let mut j = i + needle.len();
        while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
            j += 1;
        }
        if j >= bytes.len() || bytes[j] != b'=' {
            i += 1;
            continue;
        }
        j += 1;
        while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
            j += 1;
        }
        if j >= bytes.len() || (bytes[j] != b'"' && bytes[j] != b'\'') {
            i += 1;
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
        if let Ok(value) = std::str::from_utf8(&bytes[start..j]) {
            insert_symbol_from_attr(value, refs);
        }
        i = j + 1;
    }
}

/// 属性名として許可される文字か（アルファベット・数字・アンダースコア・コロン・ハイフン）。
fn is_attr_name_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b':' || b == b'-' || b == b'.'
}

/// `<com.example.View ...>` のようにドットを含むタグ名をカスタムビュー参照として抽出する。
fn collect_custom_view_tags(content: &str, refs: &mut HashSet<String>) {
    let bytes = content.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        let c = bytes[i + 1];
        // `</` や `<!`, `<?` はタグ名ではない
        if !c.is_ascii_alphabetic() && c != b'_' {
            i += 1;
            continue;
        }
        let start = i + 1;
        let mut end = start;
        while end < bytes.len() {
            let b = bytes[end];
            if b.is_ascii_alphanumeric() || b == b'.' || b == b'_' {
                end += 1;
            } else {
                break;
            }
        }
        if end > start {
            // 末尾のドットはタグ名に含まれないはずだが、念のため trim
            if let Ok(tag) = std::str::from_utf8(&bytes[start..end]) {
                let trimmed = tag.trim_end_matches('.');
                if trimmed.contains('.') {
                    insert_symbol_from_attr(trimmed, refs);
                }
            }
        }
        i = end.max(start + 1);
    }
}

/// XML 属性値から識別子を取り出して `refs` に追加する。
/// 値は `com.example.Foo` / `.Foo` / `Foo` / `Foo#bar` などの形を取りうる。
fn insert_symbol_from_attr(value: &str, refs: &mut HashSet<String>) {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return;
    }
    // `class#method` のようなリソース参照は両方を取り出す
    for segment in trimmed.split(['#', '$']) {
        let segment = segment.trim().trim_start_matches('.');
        if segment.is_empty() {
            continue;
        }
        // 末尾セグメント（例: `com.example.Foo` → `Foo`）
        if let Some(tail) = segment.rsplit('.').next()
            && is_identifier(tail)
        {
            refs.insert(tail.to_string());
        }
        // 値そのものも登録（`MainActivity` 単体の場合など）
        if is_identifier(segment) {
            refs.insert(segment.to_string());
        }
    }
}

/// トークンとして有効な識別子か（先頭が英字/`_`、続きが英数/`_`）。
fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_android_name_from_manifest() {
        let xml = r#"<manifest>
  <application>
    <activity android:name=".MainActivity" android:exported="true">
      <intent-filter>
        <action android:name="android.intent.action.MAIN" />
      </intent-filter>
    </activity>
  </application>
</manifest>"#;
        let mut refs = HashSet::new();
        extract_android_refs(xml, &mut refs);
        assert!(refs.contains("MainActivity"));
    }

    #[test]
    fn extracts_fully_qualified_class_name() {
        let xml = r#"<manifest>
  <activity android:name="com.example.app.MyActivity" />
</manifest>"#;
        let mut refs = HashSet::new();
        extract_android_refs(xml, &mut refs);
        assert!(refs.contains("MyActivity"));
    }

    #[test]
    fn extracts_tools_context() {
        let xml = r#"<LinearLayout xmlns:tools="http://schemas.android.com/tools" tools:context=".SettingsActivity"/>"#;
        let mut refs = HashSet::new();
        extract_android_refs(xml, &mut refs);
        assert!(refs.contains("SettingsActivity"));
    }

    #[test]
    fn extracts_onclick_handler() {
        let xml = r#"<Button android:onClick="onSubmit"/>"#;
        let mut refs = HashSet::new();
        extract_android_refs(xml, &mut refs);
        assert!(refs.contains("onSubmit"));
    }

    #[test]
    fn extracts_custom_view_tag() {
        let xml = r#"<com.example.views.FancyButton android:text="Hi"/>"#;
        let mut refs = HashSet::new();
        extract_android_refs(xml, &mut refs);
        assert!(refs.contains("FancyButton"));
    }

    #[test]
    fn ignores_non_android_intent_constants() {
        // android.intent.action.MAIN は class/メソッド名ではないが、末尾 `MAIN`
        // が識別子化される。これ自体は誤陽性を起こさない（そもそも MAIN という
        // シンボルが dead になっていなければ影響なし）。末尾セグメント単位で
        // 抽出される前提を確認するだけの sanity テスト。
        let xml = r#"<action android:name="android.intent.action.MAIN" />"#;
        let mut refs = HashSet::new();
        extract_android_refs(xml, &mut refs);
        assert!(refs.contains("MAIN"));
    }

    #[test]
    fn single_quoted_attribute_values_are_supported() {
        let xml = r#"<activity android:name='.Foo' />"#;
        let mut refs = HashSet::new();
        extract_android_refs(xml, &mut refs);
        assert!(refs.contains("Foo"));
    }

    #[test]
    fn similar_attribute_names_do_not_match() {
        // `app:ui_android:name=` のような偽陽性を避ける: 直前がトークン境界でない場合は無視
        let xml = r#"<foo bar:android:name="ShouldMatch" />"#;
        let mut refs = HashSet::new();
        extract_android_refs(xml, &mut refs);
        // `bar:android:name` は `android:name` とは別属性。直前がコロンなので is_attr_name_char
        // は true となり、マッチしない（偽陽性を起こさない）。
        assert!(!refs.contains("ShouldMatch"));
    }

    #[test]
    fn has_android_manifest_detects_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AndroidManifest.xml"), "<manifest/>").unwrap();
        assert!(has_android_manifest(dir.path()));
    }

    #[test]
    fn has_android_manifest_returns_false_without_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!has_android_manifest(dir.path()));
    }

    #[test]
    fn collect_xml_symbol_references_returns_empty_without_manifest() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("layout.xml"),
            r#"<LinearLayout tools:context=".Foo"/>"#,
        )
        .unwrap();
        // AndroidManifest.xml がないので空集合
        assert!(collect_xml_symbol_references(dir.path()).is_empty());
    }

    #[test]
    fn collect_xml_symbol_references_finds_activity_when_manifest_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("AndroidManifest.xml"),
            r#"<manifest><activity android:name=".HomeActivity"/></manifest>"#,
        )
        .unwrap();
        let refs = collect_xml_symbol_references(dir.path());
        assert!(refs.contains("HomeActivity"));
    }
}
