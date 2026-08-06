use super::*;

/// PHP の callable array `[Class::class, 'method']` で string ノードの中身が
/// method ref として返されることを検証 (N3 unit-level)。
#[test]
fn php_callable_array_method_segment_extracts_method_string() {
    let source =
        b"<?php\nclass C {\n    public function h() { $x = [C::class, 'foo']; return $x; }\n}\n";
    let path = camino::Utf8Path::new("dummy.php");
    let tree = parser::parse_source(source, LangId::Php).unwrap();
    let lang_id = LangId::Php;
    let _ = path; // silence unused warning
    // 再帰で array_creation_expression を探す
    fn find_array<'t>(n: tree_sitter::Node<'t>) -> Option<tree_sitter::Node<'t>> {
        if n.kind() == "array_creation_expression" {
            return Some(n);
        }
        let mut c = n.walk();
        for child in n.children(&mut c) {
            if let Some(found) = find_array(child) {
                return Some(found);
            }
        }
        None
    }
    let arr = find_array(tree.root_node()).expect("array_creation_expression must exist");
    let seg = php_callable_array_method_segment(arr, source, lang_id);
    assert!(
        seg.is_some(),
        "[C::class, 'foo'] should yield a method segment, got None"
    );
    let (m, _row, _col) = seg.unwrap();
    assert_eq!(m, "foo");
}

/// 第1要素が `Class::class` でない場合は ref として認めない (誤検出防止)
#[test]
fn php_callable_array_method_segment_rejects_non_class_const() {
    // [1, 'foo'] や ['foo', 'bar'] は callable array ではない
    let source = b"<?php\nfunction f() { $x = [1, 'foo']; return $x; }\n";
    let tree = parser::parse_source(source, LangId::Php).unwrap();
    let lang_id = LangId::Php;
    fn find_array<'t>(n: tree_sitter::Node<'t>) -> Option<tree_sitter::Node<'t>> {
        if n.kind() == "array_creation_expression" {
            return Some(n);
        }
        let mut c = n.walk();
        for child in n.children(&mut c) {
            if let Some(found) = find_array(child) {
                return Some(found);
            }
        }
        None
    }
    let arr = find_array(tree.root_node()).expect("array_creation_expression must exist");
    assert!(php_callable_array_method_segment(arr, source, lang_id).is_none());
}

/// PHP 文字列 callable `'Cls@method'` 形式で method 部分が抽出されることを検証 (N4)。
#[test]
fn php_string_callable_method_segment_extracts_pure_string() {
    let source = b"<?php\nfunction f() { return 'Controller@handle'; }\n";
    let tree = parser::parse_source(source, LangId::Php).unwrap();
    fn find_string<'t>(
        n: tree_sitter::Node<'t>,
        target: &str,
        source: &[u8],
    ) -> Option<tree_sitter::Node<'t>> {
        if n.kind() == "string" && n.utf8_text(source).ok() == Some(target) {
            return Some(n);
        }
        let mut c = n.walk();
        for child in n.children(&mut c) {
            if let Some(found) = find_string(child, target, source) {
                return Some(found);
            }
        }
        None
    }
    let s =
        find_string(tree.root_node(), "'Controller@handle'", source).expect("string must exist");
    let seg = php_string_callable_method_segment(s, source, LangId::Php);
    assert!(
        seg.is_some(),
        "'Controller@handle' should yield method segment"
    );
    let (m, _r, _c) = seg.unwrap();
    assert_eq!(m, "handle");
}

/// PHP `Cls::class . '@method'` concat 右辺 string から method 部分が抽出されることを検証 (N4)。
#[test]
fn php_string_callable_method_segment_extracts_concat_segment() {
    let source = b"<?php\nclass C {}\n$x = C::class . '@handler';\n";
    let tree = parser::parse_source(source, LangId::Php).unwrap();
    fn find_string<'t>(
        n: tree_sitter::Node<'t>,
        target: &str,
        source: &[u8],
    ) -> Option<tree_sitter::Node<'t>> {
        if n.kind() == "string" && n.utf8_text(source).ok() == Some(target) {
            return Some(n);
        }
        let mut c = n.walk();
        for child in n.children(&mut c) {
            if let Some(found) = find_string(child, target, source) {
                return Some(found);
            }
        }
        None
    }
    let s = find_string(tree.root_node(), "'@handler'", source).expect("string must exist");
    let seg = php_string_callable_method_segment(s, source, LangId::Php);
    assert!(seg.is_some(), "Cls::class . '@handler' should match");
    let (m, _r, _c) = seg.unwrap();
    assert_eq!(m, "handler");
}

/// メール風文字列は method ref として抽出しない (誤検出防止)
#[test]
fn php_string_callable_method_segment_rejects_email_like() {
    let source = b"<?php\n$x = 'user@example.com';\n";
    let tree = parser::parse_source(source, LangId::Php).unwrap();
    fn find_string<'t>(n: tree_sitter::Node<'t>) -> Option<tree_sitter::Node<'t>> {
        if n.kind() == "string" {
            return Some(n);
        }
        let mut c = n.walk();
        for child in n.children(&mut c) {
            if let Some(found) = find_string(child) {
                return Some(found);
            }
        }
        None
    }
    let s = find_string(tree.root_node()).expect("string must exist");
    assert!(php_string_callable_method_segment(s, source, LangId::Php).is_none());
}

/// `P@ssw0rd` のようなパスワード風文字列は class 部分が 1 文字で reject される
#[test]
fn php_string_callable_method_segment_rejects_short_class_part() {
    let source = b"<?php\n$x = 'P@ssw0rd';\n";
    let tree = parser::parse_source(source, LangId::Php).unwrap();
    fn find_string<'t>(n: tree_sitter::Node<'t>) -> Option<tree_sitter::Node<'t>> {
        if n.kind() == "string" {
            return Some(n);
        }
        let mut c = n.walk();
        for child in n.children(&mut c) {
            if let Some(found) = find_string(child) {
                return Some(found);
            }
        }
        None
    }
    let s = find_string(tree.root_node()).expect("string must exist");
    assert!(php_string_callable_method_segment(s, source, LangId::Php).is_none());
}

/// 引数単独の `'@method'` (concat 親ではない) は reject
#[test]
fn php_string_callable_method_segment_rejects_bare_at_method() {
    let source = b"<?php\nfunction f($x) {} f('@handler');\n";
    let tree = parser::parse_source(source, LangId::Php).unwrap();
    fn find_string<'t>(n: tree_sitter::Node<'t>) -> Option<tree_sitter::Node<'t>> {
        if n.kind() == "string" {
            return Some(n);
        }
        let mut c = n.walk();
        for child in n.children(&mut c) {
            if let Some(found) = find_string(child) {
                return Some(found);
            }
        }
        None
    }
    let s = find_string(tree.root_node()).expect("string must exist");
    assert!(php_string_callable_method_segment(s, source, LangId::Php).is_none());
}

/// PHP のメソッド呼び出しは case-insensitive に解決される。
/// 定義 `isFooBar` と呼び出し `isFoobar` (case 違い) は同一メソッドに解決される。
#[test]
fn find_references_php_method_call_case_insensitive() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Vo.php"),
        "<?php\nclass Vo {\n    public function isFooBar(): bool { return true; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("Caller.php"),
        "<?php\nclass Caller {\n    public function check(Vo $vo): bool { return $vo->isFoobar(); }\n}\n",
    )
    .unwrap();

    let refs = find_references("isFooBar", dir.path(), None).unwrap();
    let defs = refs
        .iter()
        .filter(|r| r.kind == Some(RefKind::Definition))
        .count();
    let non_defs = refs
        .iter()
        .filter(|r| r.kind != Some(RefKind::Definition))
        .count();
    assert_eq!(defs, 1, "definition should resolve, got refs={refs:?}");
    assert_eq!(
        non_defs, 1,
        "case-different method call must resolve as reference, got refs={refs:?}"
    );
}

/// 追加ファイルなし (`extra_files` 空) の場合は通常の workspace walk と同じ件数になる。
#[test]
fn count_non_definition_refs_split_without_extra_files_keeps_existing_api() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("sample.rs"),
        "pub fn helper() {}\nfn run() { helper(); }\n",
    )
    .unwrap();

    let counts = count_non_definition_refs_split_with_extra_files(
        &["helper".to_string()],
        dir.path(),
        None,
        &[],
        |_| false,
    )
    .unwrap();

    assert_eq!(counts.get("helper"), Some(&(1, 0)));
}

/// PHP の静的メソッド呼び出し (`Foo::bar()`) も case-insensitive に解決される。
#[test]
fn find_references_php_static_method_call_case_insensitive() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.php"),
        "<?php\nclass Svc {\n    public static function doIt(): void {}\n    public function run(): void { Svc::DOIT(); }\n}\n",
    )
    .unwrap();

    let refs = find_references("doIt", dir.path(), None).unwrap();
    let non_defs = refs
        .iter()
        .filter(|r| r.kind != Some(RefKind::Definition))
        .count();
    assert_eq!(
        non_defs, 1,
        "case-different static call must resolve, got refs={refs:?}"
    );
}

/// PHP のクラス名は case-insensitive。`new FOO()` / `new Foo()` が定義 `class Foo` に解決される。
#[test]
fn find_references_php_class_name_case_insensitive() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Foo.php"),
        "<?php\nclass Foo {\n    public function go(): void {}\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("use.php"),
        "<?php\nfunction make() {\n    $a = new FOO();\n    $b = new Foo();\n    return $a;\n}\n",
    )
    .unwrap();

    let refs = find_references("Foo", dir.path(), None).unwrap();
    let non_defs = refs
        .iter()
        .filter(|r| r.kind != Some(RefKind::Definition))
        .count();
    assert_eq!(
        non_defs, 2,
        "class name `new FOO`/`new Foo` must resolve case-insensitively, got refs={refs:?}"
    );
}

/// PHP のプロパティ名は case-sensitive。大小違いの検索は member_access に一致しない。
#[test]
fn find_references_php_property_access_is_case_sensitive() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.php"),
        "<?php\nclass C {\n    public int $myProp = 0;\n    public function f(): int { return $this->myProp; }\n}\n",
    )
    .unwrap();

    // case-fold していれば "MYPROP" が "myProp" に誤マッチするが、プロパティは case-sensitive。
    let refs = find_references("MYPROP", dir.path(), None).unwrap();
    assert!(
        refs.is_empty(),
        "property access is case-sensitive; uppercase search must not match, got refs={refs:?}"
    );
}

/// PHP のクラス定数は case-sensitive。大小違いの検索は class_constant_access に一致しない。
#[test]
fn find_references_php_class_constant_is_case_sensitive() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.php"),
        "<?php\nclass C {\n    const MyConst = 1;\n    public function f(): int { return self::MyConst; }\n}\n",
    )
    .unwrap();

    let refs = find_references("MYCONST", dir.path(), None).unwrap();
    assert!(
        refs.is_empty(),
        "class constant is case-sensitive; uppercase search must not match, got refs={refs:?}"
    );
}

/// バッチ参照検索 (dead-code / api が使う経路) でも PHP メソッドの case 違いが解決される。
#[test]
fn find_references_batch_php_method_case_insensitive() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Vo.php"),
        "<?php\nclass Vo {\n    public function isFooBar(): bool { return true; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("Caller.php"),
        "<?php\nclass Caller {\n    public function check(Vo $vo): bool { return $vo->isFoobar(); }\n}\n",
    )
    .unwrap();

    let map = find_references_batch(&["isFooBar".to_string()], dir.path(), None).unwrap();
    let refs = map.get("isFooBar").cloned().unwrap_or_default();
    let non_defs = refs
        .iter()
        .filter(|r| r.kind != Some(RefKind::Definition))
        .count();
    assert_eq!(
        non_defs, 1,
        "batch path must resolve case-different call, got refs={refs:?}"
    );
}

/// PHP の名前空間付きクラス参照 (`use App\Foo` / 型ヒント / `new \App\Foo()`) も
/// case-insensitive に解決される (qualified_name の末尾 name)。
#[test]
fn find_references_php_namespaced_class_case_insensitive() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Foo.php"),
        "<?php\nnamespace App\\Repo;\nclass UserRepository {\n    public function go(): void {}\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("use.php"),
        "<?php\nuse App\\Repo\\USERREPOSITORY;\nfunction make(\\App\\Repo\\Userrepository $r) {\n    return new \\App\\Repo\\userRepository();\n}\n",
    )
    .unwrap();

    // 参照: use の USERREPOSITORY / 型ヒント Userrepository / new userRepository (全て case 違い)。
    let refs = find_references("UserRepository", dir.path(), None).unwrap();
    let non_defs = refs
        .iter()
        .filter(|r| r.kind != Some(RefKind::Definition))
        .count();
    assert!(
        non_defs >= 3,
        "namespaced class refs must resolve case-insensitively, got refs={refs:?}"
    );
}

/// PHP の trait adaptation (`insteadof` / `as`) のメソッド名も case-insensitive に解決される。
#[test]
fn find_references_php_trait_adaptation_method_case_insensitive() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("t.php"),
        "<?php\ntrait A {\n    public function work(): void {}\n}\ntrait B {\n    public function work(): void {}\n}\nclass C {\n    use A, B {\n        B::WORK insteadof A;\n        A::Work as legacyWork;\n    }\n}\n",
    )
    .unwrap();

    // B::WORK (insteadof) と A::Work (as) は trait メソッド work の case 違い参照。
    let refs = find_references("work", dir.path(), None).unwrap();
    let non_defs = refs
        .iter()
        .filter(|r| r.kind != Some(RefKind::Definition))
        .count();
    assert!(
        non_defs >= 2,
        "trait adaptation method refs must resolve case-insensitively, got refs={refs:?}"
    );
}

/// PHP の `use const` (定数 import) は case-sensitive、`use` (クラス import) は case-insensitive。
#[test]
fn find_references_php_use_const_is_case_sensitive() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.php"),
        "<?php\nnamespace App;\nuse const App\\Config\\MAX_SIZE;\nuse App\\Repo\\UserRepo;\n",
    )
    .unwrap();

    // use const の定数名は case-sensitive (大小違いは一致しない)。
    assert!(
        find_references("max_size", dir.path(), None)
            .unwrap()
            .is_empty(),
        "use const must be case-sensitive"
    );
    // 対照: use (クラス import) は case-insensitive。
    assert!(
        !find_references("USERREPO", dir.path(), None)
            .unwrap()
            .is_empty(),
        "use class import must be case-insensitive"
    );
}

/// PHP の group use 内でも const は case-sensitive、クラス / 関数は case-insensitive。
#[test]
fn find_references_php_group_use_const_is_case_sensitive() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.php"),
        "<?php\nnamespace App;\nuse App\\{ Foo, const MAX_LEN, function helper };\n",
    )
    .unwrap();

    assert!(
        find_references("max_len", dir.path(), None)
            .unwrap()
            .is_empty(),
        "group use const must be case-sensitive"
    );
    assert!(
        !find_references("FOO", dir.path(), None).unwrap().is_empty(),
        "group use class must be case-insensitive"
    );
    assert!(
        !find_references("HELPER", dir.path(), None)
            .unwrap()
            .is_empty(),
        "group use function must be case-insensitive"
    );
}
