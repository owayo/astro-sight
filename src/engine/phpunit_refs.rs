//! PHPUnit の metadata (DocBlock annotations / PHP attributes) 経由で参照される
//! メソッドを「シンボル参照」として抽出する。
//!
//! PHPUnit は `@dataProvider <method>` (DocBlock) や `#[DataProvider('<method>')]`
//! (PHP attribute) で同クラス内 (または別クラス) の provider メソッドを reflection
//! 経由で呼び出す。これらは AST 上の通常呼び出しではないため、refs / dead-code が
//! 「参照ゼロ」と誤判定する原因になる。
//!
//! 本モジュールは `method_declaration` ノードを受け取り、その直前 sibling の
//! `comment` ノード (PHPDoc) と method 内 `attribute_list` 子から PHPUnit metadata
//! 由来の method 参照を抽出する。
//!
//! 抽出対象 (PHPUnit 10+ 公式 attribute と従来 DocBlock):
//! - DocBlock: `@dataProvider <method>`, `@dataProvider Class::method`,
//!   `@depends <method>`, `@depends Class::method`
//! - Attribute: `#[DataProvider('method')]`,
//!   `#[DataProviderExternal(Class::class, 'method')]`,
//!   `#[Depends('method')]`, `#[DependsExternal(Class::class, 'method')]`,
//!   `#[DependsUsingDeepClone('method')]`,
//!   `#[DependsUsingShallowClone('method')]`,
//!   `#[DependsExternalUsingDeepClone(Class::class, 'method')]`,
//!   `#[DependsExternalUsingShallowClone(Class::class, 'method')]`
//!
//! 対象外 (coverage metadata であり runtime call ではないため):
//! - `@uses`, `@covers`, `#[CoversClass]`, `#[CoversFunction]`, `#[UsesClass]`
//! - `#[TestWith]`, `#[TestWithJson]` (引数は inline literal、method 参照しない)

use tree_sitter::Node;

use crate::language::LangId;

/// `method_declaration` ノードに紐づく PHPUnit metadata から
/// method 参照候補を抽出して返す。
///
/// 戻り値の `(name, row, col)` はファイル全体での 0-indexed 位置。
/// PHP 以外 / method_declaration 以外 / metadata なしの場合は空 Vec。
pub fn phpunit_metadata_ref_segments(
    node: Node<'_>,
    source: &[u8],
    lang_id: LangId,
) -> Vec<(String, usize, usize)> {
    if lang_id != LangId::Php {
        return Vec::new();
    }
    if node.kind() != "method_declaration" {
        return Vec::new();
    }

    let mut out: Vec<(String, usize, usize)> = Vec::new();
    collect_docblock_refs(node, source, &mut out);
    collect_attribute_refs(node, source, &mut out);
    out
}

/// `method_declaration` の直前 sibling の `comment` ノードから
/// `@dataProvider <name>` / `@depends <name>` を抽出する。
///
/// 複数の comment が連続する場合もあるため、`prev_named_sibling()` を
/// `comment` でない要素に当たるまで辿る。PHPDoc (`/** ... */`) 以外は無視する。
fn collect_docblock_refs(method: Node<'_>, source: &[u8], out: &mut Vec<(String, usize, usize)>) {
    let mut sibling = method.prev_named_sibling();
    while let Some(c) = sibling {
        if c.kind() != "comment" {
            break;
        }
        let text = match c.utf8_text(source) {
            Ok(t) => t,
            Err(_) => break,
        };
        // PHPDoc (`/** ... */`) のみ対象。`//` や `/* */` を跨いだ前方の PHPDoc は
        // method に直接付いているとはみなさない (line comment や通常 block comment が
        // 間にある場合は DocBlock として扱わない PHP の慣習に沿う)。
        if !text.starts_with("/**") {
            break;
        }
        let start_pos = c.start_position();
        parse_docblock_tags(text, start_pos.row, start_pos.column, out);
        sibling = c.prev_named_sibling();
    }
}

/// DocBlock テキストから `@dataProvider <name>` / `@depends <name>` を抽出する。
///
/// `Class::method` 形式の場合は `method` 部分のみを ref として返す
/// (provider/depends 先のメソッド名を dead-code 生存判定に使うため)。
fn parse_docblock_tags(
    doc: &str,
    doc_start_row: usize,
    doc_start_col: usize,
    out: &mut Vec<(String, usize, usize)>,
) {
    const TAGS: &[&str] = &["@dataProvider", "@depends"];

    for (line_idx, line) in doc.split('\n').enumerate() {
        for tag in TAGS {
            let Some(tag_pos) = line.find(tag) else {
                continue;
            };
            // tag の直後が `(` の場合 (例: @dataProvider() といった奇妙な記法) は skip
            let after = tag_pos + tag.len();
            // tag の直後がアルファベット (例: @dataProviderX) の場合は別タグなので skip
            if let Some(next_ch) = line.as_bytes().get(after)
                && next_ch.is_ascii_alphabetic()
            {
                continue;
            }
            // 空白を skip
            let rest = &line[after..];
            let value_offset = rest
                .bytes()
                .take_while(|b| matches!(b, b' ' | b'\t'))
                .count();
            let value_start_in_line = after + value_offset;
            let rest_value = &line[value_start_in_line..];

            // 識別子の終わり (空白 / `*` / 行末) を探す
            let value_end_offset = rest_value
                .bytes()
                .take_while(|b| !b.is_ascii_whitespace() && *b != b'*')
                .count();
            if value_end_offset == 0 {
                continue;
            }
            let token = &rest_value[..value_end_offset];

            // `Class::method` なら method 部分のみ
            let method_in_token_offset = token.rfind("::").map(|p| p + 2).unwrap_or(0);
            let name = &token[method_in_token_offset..];
            if name.is_empty() {
                continue;
            }

            let name_col_in_line = value_start_in_line + method_in_token_offset;
            let row = doc_start_row + line_idx;
            let col = if line_idx == 0 {
                doc_start_col + name_col_in_line
            } else {
                name_col_in_line
            };

            out.push((name.to_string(), row, col));
        }
    }
}

/// `method_declaration` の `attribute_list` 子から PHPUnit attribute 由来の
/// method 参照を抽出する。
///
/// tree-sitter-php の `attribute_list` は内側を leaf として扱うことが多いため、
/// テキストベースのスキャンで対応する。位置は `attribute_list` の
/// `start_position` + 内側 byte offset から計算する。
fn collect_attribute_refs(method: Node<'_>, source: &[u8], out: &mut Vec<(String, usize, usize)>) {
    let mut cursor = method.walk();
    for child in method.children(&mut cursor) {
        if child.kind() != "attribute_list" {
            continue;
        }
        let Ok(text) = child.utf8_text(source) else {
            continue;
        };
        let start_pos = child.start_position();
        parse_attribute_text(text, start_pos.row, start_pos.column, out);
    }
}

/// PHPUnit attribute 名と、抽出すべき string 引数の位置 (0-indexed) のマップ。
///
/// 例: `DataProvider('method')` → 0 番目の string 引数を method ref とする。
/// 例: `DataProviderExternal(Class::class, 'method')` → 1 番目の string 引数。
fn phpunit_attribute_method_arg_index(name: &str) -> Option<usize> {
    match name {
        "DataProvider" | "Depends" => Some(0),
        "DependsUsingDeepClone" | "DependsUsingShallowClone" => Some(0),
        "DataProviderExternal" | "DependsExternal" => Some(1),
        "DependsExternalUsingDeepClone" | "DependsExternalUsingShallowClone" => Some(1),
        _ => None,
    }
}

/// `attribute_list` テキストをスキャンして PHPUnit attribute 由来の method 参照を抽出する。
///
/// PHP 8 の grouped attribute (`#[A, B('x')]`) も対応する。
/// `#[...]` の中では `,` 区切りで複数 attribute が並ぶため、attribute 1 つを処理した
/// 後に `]` が来なければ次の attribute 名を読みに行く。
fn parse_attribute_text(
    text: &str,
    attr_start_row: usize,
    attr_start_col: usize,
    out: &mut Vec<(String, usize, usize)>,
) {
    let bytes = text.as_bytes();
    let mut i = 0;
    // `[...]` group の内側か。group 内では `,` 区切りで attribute が連なる。
    let mut in_group = false;
    while i < bytes.len() {
        if !in_group {
            // group 外: `#[` を探す
            if i + 1 < bytes.len() && bytes[i] == b'#' && bytes[i + 1] == b'[' {
                i += 2;
                in_group = true;
            } else {
                i += 1;
                continue;
            }
        } else {
            // group 内: 空白 / 改行を skip
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i >= bytes.len() {
                break;
            }
            if bytes[i] == b']' {
                in_group = false;
                i += 1;
                continue;
            }
            if !(bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' || bytes[i] == b'\\') {
                // 想定外文字 (`,` 残骸など): 1 文字 skip
                i += 1;
                continue;
            }
        }

        // attribute 名を読む。qualified name (例: `PHPUnit\\Framework\\Attributes\\DataProvider`)
        // も許容するため、`\` / 英数字 / `_` を含めて取得。
        let name_start = i;
        while i < bytes.len()
            && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'\\')
        {
            i += 1;
        }
        let qualified_name = &text[name_start..i];
        let last_segment = qualified_name.rsplit('\\').next().unwrap_or(qualified_name);
        let target_idx = phpunit_attribute_method_arg_index(last_segment);

        // 名前の後の空白を skip
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }

        // `(` 引数列があれば parse、なければ次の attribute (group 内 `,`) or `]` (group 終わり) へ
        if i < bytes.len() && bytes[i] == b'(' {
            i += 1; // `(` 後
            let mut arg_idx = 0usize;
            let mut depth = 0i32; // 引数内 (`()`/`[]`) のネスト
            while i < bytes.len() {
                if depth == 0 && bytes[i] == b')' {
                    i += 1;
                    break;
                }
                if depth == 0 && bytes[i] == b',' {
                    arg_idx += 1;
                    i += 1;
                    continue;
                }
                if bytes[i] == b'\'' || bytes[i] == b'"' {
                    let quote = bytes[i];
                    let inner_start = i + 1;
                    let mut j = inner_start;
                    while j < bytes.len() {
                        if bytes[j] == b'\\' && j + 1 < bytes.len() {
                            j += 2;
                            continue;
                        }
                        if bytes[j] == quote {
                            break;
                        }
                        j += 1;
                    }
                    if let Some(idx) = target_idx
                        && depth == 0
                        && arg_idx == idx
                        && j > inner_start
                    {
                        let name_bytes = &bytes[inner_start..j];
                        if let Ok(name_str) = std::str::from_utf8(name_bytes) {
                            let (rel_row, rel_col) = byte_offset_to_row_col(text, inner_start);
                            let row = attr_start_row + rel_row;
                            let col = if rel_row == 0 {
                                attr_start_col + rel_col
                            } else {
                                rel_col
                            };
                            out.push((name_str.to_string(), row, col));
                        }
                    }
                    i = j + 1;
                    continue;
                }
                if bytes[i] == b'(' || bytes[i] == b'[' {
                    depth += 1;
                    i += 1;
                    continue;
                }
                if bytes[i] == b')' || bytes[i] == b']' {
                    // depth == 0 のケースは上で処理済みなのでここは depth > 0
                    depth -= 1;
                    i += 1;
                    continue;
                }
                i += 1;
            }
        }

        // この attribute 終わり。group 内なら `,` で次の attribute へ、`]` で group 終了。
        // 空白を skip
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b',' && in_group {
            i += 1;
            // 次反復で group 内の次の attribute を処理
            continue;
        }
        if i < bytes.len() && bytes[i] == b']' && in_group {
            in_group = false;
            i += 1;
            continue;
        }
        // それ以外は fallthrough (想定外形式)。次反復で `#[` を再度探す。
    }
}

/// `text` 内の byte offset を `(row, col)` に変換する。
/// `\n` を行区切りとして数える。
fn byte_offset_to_row_col(text: &str, byte_offset: usize) -> (usize, usize) {
    let bytes = text.as_bytes();
    let mut row = 0;
    let mut col = 0;
    let limit = byte_offset.min(bytes.len());
    for &b in &bytes[..limit] {
        if b == b'\n' {
            row += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    (row, col)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::parser;

    fn parse_php(src: &str) -> (tree_sitter::Tree, Vec<u8>) {
        let bytes = src.as_bytes().to_vec();
        let tree = parser::parse_source(&bytes, LangId::Php).expect("parse php");
        (tree, bytes)
    }

    /// 木全体から `method_declaration` を集める。
    fn collect_methods<'a>(node: tree_sitter::Node<'a>, out: &mut Vec<tree_sitter::Node<'a>>) {
        if node.kind() == "method_declaration" {
            out.push(node);
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            collect_methods(child, out);
        }
    }

    fn segments_for_first_method(src: &str) -> Vec<(String, usize, usize)> {
        let (tree, bytes) = parse_php(src);
        let mut methods = Vec::new();
        collect_methods(tree.root_node(), &mut methods);
        let m = methods.first().expect("at least one method");
        phpunit_metadata_ref_segments(*m, &bytes, LangId::Php)
    }

    #[test]
    fn phpunit_dataprovider_resolves_provider_method_from_docblock() {
        let src = r#"<?php
class T extends TestCase {
    /**
     * @dataProvider providerForValidateFormat
     */
    public function testValidations() {}

    public function providerForValidateFormat() { return []; }
}
"#;
        let segs = segments_for_first_method(src);
        let names: Vec<&str> = segs.iter().map(|(n, _, _)| n.as_str()).collect();
        assert!(names.contains(&"providerForValidateFormat"), "{names:?}");
    }

    #[test]
    fn phpunit_dataprovider_attribute_resolves_provider_method() {
        let src = r#"<?php
class T extends TestCase {
    #[DataProvider('attrProvider')]
    public function testThird() {}

    public function attrProvider() { return []; }
}
"#;
        let segs = segments_for_first_method(src);
        let names: Vec<&str> = segs.iter().map(|(n, _, _)| n.as_str()).collect();
        assert!(names.contains(&"attrProvider"), "{names:?}");
    }

    #[test]
    fn phpunit_dataprovider_external_attribute_resolves_second_string_argument() {
        let src = r#"<?php
class T extends TestCase {
    #[DataProviderExternal(\Foo::class, 'externalProvider')]
    public function testThird() {}
}
"#;
        let segs = segments_for_first_method(src);
        let names: Vec<&str> = segs.iter().map(|(n, _, _)| n.as_str()).collect();
        assert!(names.contains(&"externalProvider"), "{names:?}");
    }

    #[test]
    fn phpunit_depends_docblock_resolves_target_method() {
        let src = r#"<?php
class T extends TestCase {
    /**
     * @depends testFirst
     */
    public function testSecond() {}

    public function testFirst() {}
}
"#;
        let segs = segments_for_first_method(src);
        let names: Vec<&str> = segs.iter().map(|(n, _, _)| n.as_str()).collect();
        assert!(names.contains(&"testFirst"), "{names:?}");
    }

    #[test]
    fn phpunit_depends_attribute_resolves_target_method() {
        let src = r#"<?php
class T extends TestCase {
    #[Depends('testDep')]
    public function testSecond() {}

    public function testDep() {}
}
"#;
        let segs = segments_for_first_method(src);
        let names: Vec<&str> = segs.iter().map(|(n, _, _)| n.as_str()).collect();
        assert!(names.contains(&"testDep"), "{names:?}");
    }

    #[test]
    fn phpunit_depends_using_deep_clone_attribute_resolves_first_argument() {
        let src = r#"<?php
class T extends TestCase {
    #[DependsUsingDeepClone('producer')]
    public function testConsumer() {}
}
"#;
        let segs = segments_for_first_method(src);
        let names: Vec<&str> = segs.iter().map(|(n, _, _)| n.as_str()).collect();
        assert!(names.contains(&"producer"), "{names:?}");
    }

    #[test]
    fn phpunit_qualified_attribute_name_is_recognized() {
        // PHPUnit\Framework\Attributes\DataProvider のような qualified name も
        // 末尾セグメント DataProvider で認識される
        let src = r#"<?php
class T extends TestCase {
    #[\PHPUnit\Framework\Attributes\DataProvider('qualifiedProvider')]
    public function testThird() {}
}
"#;
        let segs = segments_for_first_method(src);
        let names: Vec<&str> = segs.iter().map(|(n, _, _)| n.as_str()).collect();
        assert!(names.contains(&"qualifiedProvider"), "{names:?}");
    }

    #[test]
    fn phpunit_class_qualified_dataprovider_in_docblock_returns_method_part_only() {
        // @dataProvider Class::method の場合は method 部分のみ抽出
        let src = r#"<?php
class T extends TestCase {
    /**
     * @dataProvider SomeProvider::providerMethod
     */
    public function testValidations() {}
}
"#;
        let segs = segments_for_first_method(src);
        let names: Vec<&str> = segs.iter().map(|(n, _, _)| n.as_str()).collect();
        assert!(names.contains(&"providerMethod"), "{names:?}");
        // Class 名は含まれないこと
        assert!(!names.contains(&"SomeProvider"), "{names:?}");
    }

    #[test]
    fn phpunit_covers_and_uses_are_ignored() {
        // @covers / @uses / #[CoversClass] / #[UsesClass] は coverage metadata で
        // runtime call ではないため抽出対象外
        let src = r#"<?php
class T extends TestCase {
    /**
     * @covers ClassUnderTest
     * @uses HelperClass::helperMethod
     */
    #[CoversClass(ClassUnderTest::class)]
    #[UsesClass(HelperClass::class)]
    public function testValidations() {}
}
"#;
        let segs = segments_for_first_method(src);
        let names: Vec<&str> = segs.iter().map(|(n, _, _)| n.as_str()).collect();
        assert!(!names.contains(&"ClassUnderTest"), "{names:?}");
        assert!(!names.contains(&"HelperClass"), "{names:?}");
        assert!(!names.contains(&"helperMethod"), "{names:?}");
    }

    #[test]
    fn phpunit_non_php_language_returns_empty() {
        let src = "trap 'cleanup' INT\n";
        let (tree, bytes) = parse_php("<?php class T { public function f() {} }");
        let mut methods = Vec::new();
        collect_methods(tree.root_node(), &mut methods);
        let m = *methods.first().expect("method");
        let segs = phpunit_metadata_ref_segments(m, &bytes, LangId::Bash);
        assert!(segs.is_empty(), "{segs:?}");
        // src は warning 抑制のため未使用にしない (将来用)
        let _ = src;
    }

    #[test]
    fn phpunit_non_method_node_returns_empty() {
        let src = r#"<?php class T extends TestCase {
    /** @dataProvider provider */
    public function testValidations() {}
}
"#;
        let (tree, bytes) = parse_php(src);
        // root (program) ノードを渡すと空
        let segs = phpunit_metadata_ref_segments(tree.root_node(), &bytes, LangId::Php);
        assert!(segs.is_empty(), "{segs:?}");
    }

    #[test]
    fn phpunit_metadata_refs_report_original_string_position() {
        // DocBlock 内の `@dataProvider providerForValidateFormat` の
        // `providerForValidateFormat` の位置がファイル全体の (row, col) になる
        let src = "<?php\nclass T extends TestCase {\n    /**\n     * @dataProvider providerForValidateFormat\n     */\n    public function testValidations() {}\n}\n";
        let segs = segments_for_first_method(src);
        let (name, row, col) = segs
            .iter()
            .find(|(n, _, _)| n == "providerForValidateFormat")
            .expect("found");
        // line 3 (0-indexed) の `     * @dataProvider providerForValidateFormat`
        // col は ` ` 5個 + `* ` 2個 + `@dataProvider ` 14個 = 21
        assert_eq!(*row, 3, "row should be doc line index, got {row}");
        assert_eq!(*col, 21, "col mismatch for {name}");
    }

    #[test]
    fn phpunit_dataprovider_external_with_class_class_skips_first_arg() {
        // 1 番目の引数 (Class::class) は skip し、2 番目の string を取る
        let src = r#"<?php
class T extends TestCase {
    #[DataProviderExternal(\Foo\Bar::class, 'externalOne')]
    public function testThird() {}
}
"#;
        let segs = segments_for_first_method(src);
        let names: Vec<&str> = segs.iter().map(|(n, _, _)| n.as_str()).collect();
        assert!(names.contains(&"externalOne"), "{names:?}");
    }

    #[test]
    fn phpunit_grouped_attributes_resolve_all_method_refs() {
        // PHP 8 の grouped attribute: `#[A, B]` の 2 個目以降も解析する
        let src = r#"<?php
class T extends TestCase {
    #[DataProvider('a'), DataProvider('b')]
    public function testGrouped() {}
}
"#;
        let segs = segments_for_first_method(src);
        let names: Vec<&str> = segs.iter().map(|(n, _, _)| n.as_str()).collect();
        assert!(names.contains(&"a"), "{names:?}");
        assert!(names.contains(&"b"), "{names:?}");
    }

    #[test]
    fn phpunit_grouped_attributes_mixed_recognized_and_unknown() {
        // grouped 内に PHPUnit 以外の attribute が混じっても、PHPUnit のものだけ拾う
        let src = r#"<?php
class T extends TestCase {
    #[SomeOtherAttr, DataProvider('groupedProvider')]
    public function testGrouped() {}
}
"#;
        let segs = segments_for_first_method(src);
        let names: Vec<&str> = segs.iter().map(|(n, _, _)| n.as_str()).collect();
        assert!(names.contains(&"groupedProvider"), "{names:?}");
    }

    #[test]
    fn phpunit_docblock_stops_at_non_phpdoc_comment() {
        // method 直前 sibling が line comment (`//`) なら、その先の PHPDoc は
        // method に付いた DocBlock とはみなさない (PHP の慣習に沿う)。
        // tree-sitter-php は `//` line comment も `comment` ノードとして扱う。
        let src = "<?php
class T extends TestCase {
    /**
     * @dataProvider farProvider
     */
    // regular line comment between docblock and method
    public function testValidations() {}
}
";
        let segs = segments_for_first_method(src);
        let names: Vec<&str> = segs.iter().map(|(n, _, _)| n.as_str()).collect();
        assert!(
            !names.contains(&"farProvider"),
            "non-PHPDoc comment should block DocBlock lookup: {names:?}"
        );
    }
}
