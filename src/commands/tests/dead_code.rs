//! dead-code 検出と、その diff スコープ絞り込みのテスト。

#[allow(unused_imports)]
use crate::commands::tests::common::*;
#[allow(unused_imports)]
use crate::commands::*;
#[allow(unused_imports)]
use crate::models::review::{
    ApiChanges, ApiSymbol, ApiSymbolChange, CompatibleApiModification, MissingCochange,
    MovedSymbol, PropertyToFieldChange, ReviewResult,
};
#[allow(unused_imports)]
use std::collections::HashSet;
#[allow(unused_imports)]
use std::fs;
#[allow(unused_imports)]
use std::io::Cursor;
#[allow(unused_imports)]
use std::process::Command;

/// Angular component の public method が `templateUrl` で紐づく
/// `.component.html` から参照されている場合、dead 判定から除外される。
///
/// 再現元: astro-sight-bug-reports#4 (framework-template-ref)
/// - `@Component({ templateUrl: './foo.component.html' })` で紐づく HTML 内の
///   `(event)="method()"` / `[prop]="method()"` / `[ngStyle]="{ ...: method() }"`
///   等の binding 式で呼ばれている component method が
///   TS AST だけ見ると dead 扱いされる問題を修正。
#[test]
fn detect_dead_excludes_angular_component_methods_referenced_from_template() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    // Angular プロジェクト標識として angular.json を置く
    fs::write(repo.join("angular.json"), "{}").expect("write angular.json");

    let component_ts = r#"
import { Component } from '@angular/core';

@Component({
    selector: 'app-sample',
    templateUrl: './sample.component.html',
})
export class SampleComponent {
    public headerCheck: boolean = false;

    public headerCheckChanged(): void {
    }

    public isHeaderDisabled(): boolean {
        return false;
    }

    public reallyUnusedMethod(): void {
    }
}
"#;
    let component_html = r#"
<label [ngStyle]="{'display': isHeaderDisabled() ? 'none' : ''}">
    <input type="checkbox"
           [(ngModel)]="headerCheck"
           (ngModelChange)="headerCheckChanged()">
</label>
"#;
    fs::write(repo.join("sample.component.ts"), component_ts).expect("write ts");
    fs::write(repo.join("sample.component.html"), component_html).expect("write html");

    let files = vec![repo.join("sample.component.ts")];
    let (dead, _test_only) =
        detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
    let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

    assert!(
        !names.iter().any(|n| n.ends_with("headerCheckChanged")),
        "Angular template から (ngModelChange) で参照される method は dead から除外されるべき。got: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.ends_with("isHeaderDisabled")),
        "Angular template の [ngStyle] 式から参照される method は dead から除外されるべき。got: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.ends_with("reallyUnusedMethod")),
        "テンプレートからも参照されない method は dead として検出されるべき。got: {names:?}"
    );
}

#[test]
fn detect_dead_php_duplicate_static_factory_methods_are_owner_aware() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    fs::write(
        repo.join("A.php"),
        "<?php\nclass A {\n    public static function new(): self { return new self(); }\n}\n",
    )
    .expect("write A");
    fs::write(
        repo.join("B.php"),
        "<?php\nclass B {\n    public static function new(): self { return new self(); }\n}\n",
    )
    .expect("write B");
    fs::write(
        repo.join("use.php"),
        "<?php\nfunction use_classes(): void {\n    $a = new A();\n    $b = new B();\n    A::new();\n}\n",
    )
    .expect("write use");

    let files = vec![repo.join("A.php"), repo.join("B.php"), repo.join("use.php")];
    let (dead, _test_only) =
        detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
    let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

    assert!(
        !names.contains(&"A.new"),
        "A::new() で参照される A.new は dead ではない。got: {names:?}"
    );
    assert!(
        names.contains(&"B.new"),
        "同名 factory が複数 owner にあっても未参照の B.new は dead として検出する。got: {names:?}"
    );
}

#[test]
fn detect_dead_php_duplicate_static_factory_methods_on_single_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    fs::write(
        repo.join("A.php"),
        "<?php\nclass A { public static function new(): self { return new self(); } }\n",
    )
    .expect("write A");
    fs::write(
        repo.join("B.php"),
        "<?php\nclass B { public static function new(): self { return new self(); } }\n",
    )
    .expect("write B");
    fs::write(
        repo.join("use.php"),
        "<?php\nfunction use_classes(): void { $a = new A(); $b = new B(); A::new(); }\n",
    )
    .expect("write use");

    let files = vec![repo.join("A.php"), repo.join("B.php"), repo.join("use.php")];
    let (dead, _test_only) =
        detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
    let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

    assert!(
        !names.contains(&"A.new") && names.contains(&"B.new"),
        "1行 class 定義でも owner-aware に PHP factory method を判定する。got: {names:?}"
    );
}

#[test]
fn detect_dead_php_duplicate_methods_with_dynamic_call_remain_ambiguous() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    fs::write(
        repo.join("A.php"),
        "<?php\nclass A {\n    public static function new(): self { return new self(); }\n}\n",
    )
    .expect("write A");
    fs::write(
        repo.join("B.php"),
        "<?php\nclass B {\n    public static function new(): self { return new self(); }\n}\n",
    )
    .expect("write B");
    fs::write(
        repo.join("use.php"),
        "<?php\nfunction use_classes($factory): void {\n    $a = new A();\n    $b = new B();\n    $factory->new();\n}\n",
    )
    .expect("write use");

    let files = vec![repo.join("A.php"), repo.join("B.php"), repo.join("use.php")];
    let (dead, _test_only) =
        detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
    let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

    assert!(
        !names.contains(&"A.new") && !names.contains(&"B.new"),
        "動的呼び出し $factory->new() は owner を確定できないため旧スキップを維持する。got: {names:?}"
    );
}

#[test]
fn detect_dead_cpp_h_header_class_methods_are_parsed_as_cpp() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    fs::write(
        repo.join("GenericClient.h"),
        "class GenericClient {\n\
public:\n\
    int getAdditionalHttpHeaders() const { return 0; }\n\
    int getSetupFormat() const { return 1; }\n\
};\n",
    )
    .expect("write header");

    let files = vec![repo.join("GenericClient.h")];
    let (dead, _test_only) =
        detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
    let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

    assert!(
        names.contains(&"GenericClient.getAdditionalHttpHeaders")
            && names.contains(&"GenericClient.getSetupFormat"),
        ".h 内の C++ public getter も dead として検出する。got: {names:?}"
    );
}

/// C/C++ の前方宣言・opaque tag (`typedef struct st_mysql MYSQL;` の `st_mysql`) は
/// 「定義」ではなく宣言なので dead_symbols に含めない。本体を持つ未使用 struct は
/// 引き続き dead として検出される (Issue #11)。
#[test]
fn detect_dead_cpp_forward_declaration_tag_excluded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let header = "typedef struct st_mysql MYSQL;\nstruct UnusedDefined { int x; };\n";
    fs::write(repo.join("mysql_service.h"), header).expect("write header");

    let files = vec![repo.join("mysql_service.h")];
    let (dead, _test_only) =
        detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
    let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

    assert!(
        !names.contains(&"st_mysql"),
        "前方宣言タグ st_mysql は dead に含めない: {names:?}"
    );
    assert!(
        names.contains(&"UnusedDefined"),
        "本体を持つ未使用 struct UnusedDefined は dead として検出されるべき: {names:?}"
    );
}

/// C/C++ の `struct X` 型使用 (引数型 / 変数宣言 / メンバ宣言 / sizeof / cast) は
/// tag 名 `X` の非 Definition 参照として数え、使用中 struct を dead に出さない。
/// GitLab #28 の C/C++ struct 型参照取りこぼしの回帰テスト。
#[test]
fn detect_dead_cpp_struct_tag_type_uses_are_live() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let c_source = "struct voice_options { const char* host; int port; };\n\
struct unused_c_struct { int x; };\n\
static void load_config_file(struct voice_options* option) {\n\
    option->port = 8080;\n\
}\n\
int main(void) {\n\
    struct voice_options voice_option = { .host = \"localhost\", .port = 80 };\n\
    load_config_file(&voice_option);\n\
    return voice_option.port;\n\
}\n";
    let cpp_source = "struct text_server_data { int code; };\n\
struct buffer_data { int size; };\n\
struct unused_cpp_struct { int y; };\n\
class Converter {\n\
    struct buffer_data* buffer;\n\
};\n\
bool read_params_generic(struct text_server_data header) {\n\
    return header.code > 0;\n\
}\n\
int allocate_buffer(void* raw) {\n\
    struct buffer_data* p = (struct buffer_data*)raw;\n\
    return p ? (int)sizeof(struct buffer_data) : 0;\n\
}\n";
    fs::write(repo.join("app_textserver.c"), c_source).expect("write c");
    fs::write(repo.join("VoiceToTextConvertServer.cpp"), cpp_source).expect("write cpp");

    let files = vec![
        repo.join("app_textserver.c"),
        repo.join("VoiceToTextConvertServer.cpp"),
    ];
    let (dead, _test_only) =
        detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
    let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

    for live in ["voice_options", "text_server_data", "buffer_data"] {
        assert!(
            !names.contains(&live),
            "型として使用中の struct {live} は dead に出さない: {names:?}"
        );
    }
    for unused in ["unused_c_struct", "unused_cpp_struct"] {
        assert!(
            names.contains(&unused),
            "未使用 struct {unused} は引き続き dead として検出されるべき: {names:?}"
        );
    }
}

/// C/C++ の enum は、型名が直接使われなくても列挙子のいずれかが参照されていれば live と
/// 判定する。body あり typedef tag も alias 名経由の参照で live と判定する。列挙子も alias も
/// 未使用なら dead として検出される (Issue #12 enumerator liveness / Issue #11 typedef alias)。
#[test]
fn detect_dead_cpp_enum_enumerator_and_typedef_alias_liveness() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let header = "enum StdAgentSatus { POST_WORK = 1, LOGOFF = 10 };\n\
enum UnusedEnum { UE_A = 1, UE_B = 2 };\n\
typedef struct st_local { int v; } LocalAlias;\n\
typedef struct st_unused { int w; } UnusedAlias;\n";
    let main_cpp = "#include \"svc.h\"\n\
int useThem() {\n\
    int x = LOGOFF;\n\
    LocalAlias la;\n\
    la.v = 1;\n\
    return x + la.v;\n\
}\n";
    git_commit_files(
        repo,
        &[("svc.h", header), ("main.cpp", main_cpp)],
        "initial",
    );

    let files = vec![repo.join("svc.h"), repo.join("main.cpp")];
    let (dead, _test_only) =
        detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
    let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

    assert!(
        !names.contains(&"StdAgentSatus"),
        "列挙子 LOGOFF が使用中の enum StdAgentSatus は dead に出さない: {names:?}"
    );
    assert!(
        !names.contains(&"st_local"),
        "alias LocalAlias が使用中の typedef tag st_local は dead に出さない: {names:?}"
    );
    assert!(
        names.contains(&"UnusedEnum"),
        "列挙子も未使用の enum UnusedEnum は dead として検出されるべき: {names:?}"
    );
    assert!(
        names.contains(&"st_unused"),
        "alias 未使用の typedef tag st_unused は dead として検出されるべき: {names:?}"
    );
}

/// codex 指摘の回帰: (1) typedef の配列長式で参照される列挙子は def 誤判定されず enum が
/// live、(2) 複数 declarator (`typedef S A, *B;`) のいずれかの alias 使用で underlying tag が
/// live と判定される (Issue #11/#12)。
#[test]
fn detect_dead_cpp_typedef_array_size_and_multiple_declarators() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let header = "enum Sz { SZ_VAL = 4 };\n\
typedef int IntArr[SZ_VAL];\n\
typedef struct st_multi { int v; } MultiA, *MultiBPtr;\n\
typedef struct st_solo { int w; } SoloAlias;\n";
    let main_cpp = "#include \"svc.h\"\n\
IntArr g_arr;\n\
int useMulti() {\n\
    MultiBPtr p = nullptr;\n\
    return p ? 1 : 0;\n\
}\n";
    git_commit_files(
        repo,
        &[("svc.h", header), ("main.cpp", main_cpp)],
        "initial",
    );

    let files = vec![repo.join("svc.h"), repo.join("main.cpp")];
    let (dead, _test_only) =
        detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
    let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

    assert!(
        !names.contains(&"Sz"),
        "typedef 配列長 IntArr[SZ_VAL] で参照される列挙子の enum Sz は live: {names:?}"
    );
    assert!(
        !names.contains(&"st_multi"),
        "複数 declarator の 2 番目 alias MultiBPtr 使用で st_multi は live: {names:?}"
    );
    assert!(
        names.contains(&"st_solo"),
        "alias SoloAlias 未使用の st_solo は dead として検出されるべき: {names:?}"
    );
}

/// Angular の inline template (`@Component({ template: \`...\` })`) で参照される
/// component method も dead 判定から除外される。
#[test]
fn detect_dead_excludes_angular_inline_template_method_refs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    fs::write(repo.join("angular.json"), "{}").expect("write angular.json");

    let component_ts = r#"
import { Component } from '@angular/core';

@Component({
    selector: 'app-inline',
    template: `<button (click)="onClick()">{{ greeting }}</button>`,
})
export class InlineComponent {
    public greeting: string = 'hi';

    public onClick(): void {
    }

    public reallyUnusedInline(): void {
    }
}
"#;
    fs::write(repo.join("inline.component.ts"), component_ts).expect("write ts");

    let files = vec![repo.join("inline.component.ts")];
    let (dead, _test_only) =
        detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
    let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

    assert!(
        !names.iter().any(|n| n.ends_with("onClick")),
        "inline template の (click) で参照される method は dead から除外されるべき。got: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.ends_with("reallyUnusedInline")),
        "inline template からも参照されない method は dead として検出されるべき。got: {names:?}"
    );
}

/// GitLab issue #8 再現: `@Component` / `@Directive` 装飾クラスの Angular ライフサイクル
/// フック (`ngAfterViewChecked` 等) は Angular ランタイムが change detection サイクルで
/// 自動呼出するため、静的解析で caller が見つからなくても dead 判定しない。
#[test]
fn detect_dead_excludes_angular_component_lifecycle_hooks() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    fs::write(repo.join("angular.json"), "{}").expect("write angular.json");

    let component_ts = r#"
import { Component } from '@angular/core';

@Component({
    template: '<div>example</div>',
})
export class MinimalComponent {
    public ngOnInit(): void {}
    public ngAfterViewChecked(): void {}
    public ngOnDestroy(): void {}

    public reallyUnused(): void {}
}
"#;
    fs::write(repo.join("minimal.component.ts"), component_ts).expect("write ts");

    let files = vec![repo.join("minimal.component.ts")];
    let (dead, _test_only) =
        detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
    let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

    for hook in ["ngOnInit", "ngAfterViewChecked", "ngOnDestroy"] {
        assert!(
            !names.iter().any(|n| n.ends_with(hook)),
            "Angular @Component の lifecycle hook {hook} は dead から除外されるべき。got: {names:?}"
        );
    }
    assert!(
        names.iter().any(|n| n.ends_with("reallyUnused")),
        "Angular component の lifecycle hook 以外の未参照 method は引き続き dead として検出されるべき。got: {names:?}"
    );
}

/// `@Directive` 装飾クラスでも lifecycle hook を dead から除外する。
#[test]
fn detect_dead_excludes_angular_directive_lifecycle_hooks() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    fs::write(repo.join("angular.json"), "{}").expect("write angular.json");

    let directive_ts = r#"
import { Directive } from '@angular/core';

@Directive({ selector: '[appFoo]' })
export class FooDirective {
    public ngOnInit(): void {}
    public ngOnChanges(): void {}
}
"#;
    fs::write(repo.join("foo.directive.ts"), directive_ts).expect("write ts");

    let files = vec![repo.join("foo.directive.ts")];
    let (dead, _test_only) =
        detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
    let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

    for hook in ["ngOnInit", "ngOnChanges"] {
        assert!(
            !names.iter().any(|n| n.ends_with(hook)),
            "Angular @Directive の lifecycle hook {hook} は dead から除外されるべき。got: {names:?}"
        );
    }
}

/// `@Component` / `@Directive` のいずれも持たないクラスで同名メソッドを定義した場合は
/// dead から除外せず引き続き検出対象とする (誤除外の防止)。
#[test]
fn detect_dead_keeps_non_angular_class_methods_with_lifecycle_names() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    // Angular プロジェクトとして認識されるよう angular.json を置く (誤除外の境界確認)
    fs::write(repo.join("angular.json"), "{}").expect("write angular.json");

    let plain_ts = r#"
export class PlainClass {
    public ngOnInit(): void {}
    public ngAfterViewChecked(): void {}
}
"#;
    fs::write(repo.join("plain.ts"), plain_ts).expect("write ts");

    let files = vec![repo.join("plain.ts")];
    let (dead, _test_only) =
        detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
    let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

    for hook in ["ngOnInit", "ngAfterViewChecked"] {
        assert!(
            names.iter().any(|n| n.ends_with(hook)),
            "@Component / @Directive を持たないクラスの {hook} は引き続き dead として検出されるべき。got: {names:?}"
        );
    }
}

/// 非 Angular プロジェクトでは `.html` ファイルを参照源としてスキャンしない
/// （誤って HTML 内の単語を参照と誤認しないことの確認）。
#[test]
fn detect_dead_does_not_use_html_in_non_angular_project() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    // angular.json も *.component.ts もない通常の TS プロジェクト
    let ts = r#"
export function ghostHandler(): void {
}
"#;
    fs::write(repo.join("util.ts"), ts).expect("write ts");
    fs::write(
        repo.join("page.html"),
        r#"<button (click)="ghostHandler()">x</button>"#,
    )
    .expect("write html");

    let files = vec![repo.join("util.ts")];
    let (dead, _test_only) =
        detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
    let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

    assert!(
        names.contains(&"ghostHandler"),
        "Angular マーカーが無い場合は HTML 参照を生存判定に使わない (非 Angular なので) 。got: {names:?}"
    );
}

/// dead-code 検出でも同じマーカーで生成ファイルは除外される
#[test]
fn detect_dead_symbols_skips_auto_generated_marker_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    fs::write(
        repo.join("gen.py"),
        "# Automatically generated by tree-sitter\ndef unused_gen():\n    pass\n",
    )
    .expect("write");
    fs::write(repo.join("hand.py"), "def unused_hand():\n    pass\n").expect("write");

    let files = vec![repo.join("gen.py"), repo.join("hand.py")];
    let (dead, _test_only) =
        detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
    let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();

    assert!(
        !names.contains(&"unused_gen"),
        "自動生成マーカーのあるファイルは dead-code 検出から除外されるべき。got: {names:?}"
    );
    assert!(
        names.contains(&"unused_hand"),
        "通常ファイルの未使用関数は dead として検出されるべき。got: {names:?}"
    );
}

/// JVM/Gradle 標準の `src/test/` 配下は dead-code 検出から既定で除外される。
/// (レポート 2026-05-21-junit-kotlin-test-dead-symbols.md の再現)
///
/// 2026-04-29 時点の resolved コメントでは「dead 側は既に `test` セグメントで除外済み」と
/// されていたが、当時の `DEFAULT_DEAD_CODE_EXCLUDES_TESTS` に `test` 単数形は無く、
/// API 検出側の `is_test_path` のみが `test` を扱っていた。本テストはこのねじれ解消の
/// 回帰防止: `test` / `androidTest` / `sharedTest` / `integrationTest` セグメントは
/// 共通定数 `TEST_DIRECTORY_SEGMENTS` 経由で dead-code 側でも既定除外されるべき。
#[test]
fn filter_diff_files_for_dead_code_excludes_jvm_src_test_directory() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    std::fs::create_dir_all(repo.join("app/src/test/java/com/example")).expect("mkdir src/test");
    std::fs::write(
        repo.join("app/src/test/java/com/example/FooTest.kt"),
        "package com.example\nclass FooTest\n",
    )
    .expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "app/src/test/java/com/example/FooTest.kt".to_string(),
        new_path: "app/src/test/java/com/example/FooTest.kt".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 2,
        }],
        deleted_old_source: None,
    }];

    let canonical = std::fs::canonicalize(repo).expect("canonicalize");
    // --include-tests なし (既定): DEFAULT_DEAD_CODE_EXCLUDES_TESTS を適用
    let excludes = resolve_dead_code_excludes(false, false, false);
    let files = filter_diff_files_for_dead_code(&canonical, &diff_files, &excludes, &[], None)
        .expect("filter");

    assert!(
        files.is_empty(),
        "JVM/Gradle 標準の src/test/ 配下は --include-tests なしで dead-code 対象から除外されるべき。got: {files:?}"
    );
}

/// `--include-tests` を opt-in した場合は JVM の `src/test/` 配下も走査対象に残る。
/// (上記 `filter_diff_files_for_dead_code_excludes_jvm_src_test_directory` の対照)
#[test]
fn filter_diff_files_for_dead_code_includes_jvm_src_test_directory_when_opted_in() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    std::fs::create_dir_all(repo.join("app/src/test/java/com/example")).expect("mkdir src/test");
    std::fs::write(
        repo.join("app/src/test/java/com/example/FooTest.kt"),
        "package com.example\nclass FooTest\n",
    )
    .expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "app/src/test/java/com/example/FooTest.kt".to_string(),
        new_path: "app/src/test/java/com/example/FooTest.kt".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 2,
        }],
        deleted_old_source: None,
    }];

    let canonical = std::fs::canonicalize(repo).expect("canonicalize");
    // --include-tests opt-in: DEFAULT_DEAD_CODE_EXCLUDES_TESTS を適用しない
    let excludes = resolve_dead_code_excludes(false, true, false);
    let files = filter_diff_files_for_dead_code(&canonical, &diff_files, &excludes, &[], None)
        .expect("filter");

    assert_eq!(
        files.len(),
        1,
        "--include-tests 時は src/test/ 配下も走査対象に残るべき。got: {files:?}"
    );
}

/// 親ディレクトリ自体に `test` セグメントが含まれていても、root 配下の通常ファイルは
/// 除外されない。`canonical_dir.join(new_path)` 後の絶対パスを判定材料にしていた
/// 過去実装では `/private/tmp/test/<repo>/src/lib.rs` が全部除外される false negative
/// が出た (2026-05-21 codex コミット前レビュー指摘)。除外判定は workspace 相対の
/// `new_path` で行うべき。
#[test]
fn filter_diff_files_for_dead_code_does_not_misclassify_when_ancestor_dir_contains_test_segment() {
    let dir = tempfile::tempdir().expect("tempdir");
    // tempdir 直下にさらに "test" セグメントの親ディレクトリを作って、そこにリポを置く
    let repo = dir.path().join("test/myrepo");
    std::fs::create_dir_all(repo.join("src")).expect("mkdir src");
    std::fs::write(
        repo.join("src/lib.rs"),
        "pub fn existing() {}\npub fn newly_dead() {}\n",
    )
    .expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/lib.rs".to_string(),
        new_path: "src/lib.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 2,
        }],
        deleted_old_source: None,
    }];

    let canonical = std::fs::canonicalize(&repo).expect("canonicalize");
    let excludes = resolve_dead_code_excludes(false, false, false);
    let files = filter_diff_files_for_dead_code(&canonical, &diff_files, &excludes, &[], None)
        .expect("filter");

    assert_eq!(
        files.len(),
        1,
        "親パスが `/.../test/myrepo` でも、リポ内 `src/lib.rs` は除外されないべき。got: {files:?}"
    );
}

/// Android instrumentation tests (`src/androidTest/`) も既定除外。
#[test]
fn filter_diff_files_for_dead_code_excludes_android_test_source_set() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    std::fs::create_dir_all(repo.join("app/src/androidTest/java/com/example"))
        .expect("mkdir androidTest");
    std::fs::write(
        repo.join("app/src/androidTest/java/com/example/InstrumentedTest.kt"),
        "package com.example\nclass InstrumentedTest\n",
    )
    .expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "app/src/androidTest/java/com/example/InstrumentedTest.kt".to_string(),
        new_path: "app/src/androidTest/java/com/example/InstrumentedTest.kt".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 2,
        }],
        deleted_old_source: None,
    }];

    let canonical = std::fs::canonicalize(repo).expect("canonicalize");
    let excludes = resolve_dead_code_excludes(false, false, false);
    let files = filter_diff_files_for_dead_code(&canonical, &diff_files, &excludes, &[], None)
        .expect("filter");

    assert!(
        files.is_empty(),
        "Android `src/androidTest/` も既定で dead-code 対象から除外されるべき。got: {files:?}"
    );
}

/// TS/JS の constructor は dead 候補から除外される。
/// (レポート 2026-04-29-typescript-constructor-implicit-call.md の再現)
/// `new ClassName(...)` で暗黙的に呼ばれるため、`refs --name constructor` で
/// 見つからず dead 判定される問題への対応。
#[test]
fn detect_dead_excludes_typescript_constructor() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();

    std::fs::write(
            repo.join("foo.ts"),
            "export class Foo {\n  constructor(public name: string) {}\n  greet() { return this.name; }\n}\n",
        )
        .expect("write");
    std::fs::write(
        repo.join("usage.ts"),
        "import { Foo } from './foo';\nconst f = new Foo('world');\nconsole.log(f.greet());\n",
    )
    .expect("write");

    let candidates =
        extract_dead_code_candidates_from_file(repo.to_str().expect("utf-8 path"), "foo.ts")
            .expect("candidates");
    let names: Vec<&str> = candidates
        .iter()
        .map(|(name, _, _)| name.as_str())
        .collect();
    assert!(
        !names
            .iter()
            .any(|n| n.ends_with(".constructor") || *n == "constructor"),
        "TS の constructor は dead 候補に含めない。got: {names:?}"
    );
    assert!(
        names.contains(&"Foo"),
        "クラス自体は dead 候補に含まれる。got: {names:?}"
    );
}

/// PHP のメソッド名は case-insensitive。case 違い (`isLocalLInk` 定義 / `isLocalLink`
/// 呼び出し) で参照される public メソッドを dead_symbols に出さない (GitLab #10 の再現)。
#[test]
fn detect_dead_php_case_insensitive_method_call_is_not_dead() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();

    std::fs::write(
        repo.join("Vo.php"),
        "<?php\nclass Vo {\n    public function isLocalLInk(): bool { return true; }\n}\n",
    )
    .expect("write");
    std::fs::write(
            repo.join("Caller.php"),
            "<?php\nclass Caller {\n    public function check(Vo $vo): bool { return $vo->isLocalLink(); }\n}\n",
        )
        .expect("write");

    let files = vec![repo.join("Vo.php"), repo.join("Caller.php")];
    let (dead, _test_only) =
        detect_dead_symbols_from_files(repo.to_str().expect("utf-8 path"), &files);
    let dead_names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();
    assert!(
        !dead_names.iter().any(|n| n.ends_with("isLocalLInk")),
        "case 違いで呼ばれる method を dead にしない。got: {dead_names:?}"
    );
}

/// dead-code --glob が positive whitelist として絞り込みに使われていることを確認する。
/// 以前は Match::None も許可されており、`**/*.py` 指定でも Rust ファイル等が残っていた。
#[test]
fn filter_diff_files_for_dead_code_glob_acts_as_whitelist() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    // glob による絞り込みの単体検証なので、実ファイルは作らず diff 模擬のみ。
    let diff_files = vec![
        crate::models::impact::DiffFile {
            old_path: "/dev/null".to_string(),
            new_path: "src/foo.rs".to_string(),
            hunks: Vec::new(),
            deleted_old_source: None,
        },
        crate::models::impact::DiffFile {
            old_path: "/dev/null".to_string(),
            new_path: "src/bar.py".to_string(),
            hunks: Vec::new(),
            deleted_old_source: None,
        },
    ];

    let files = filter_diff_files_for_dead_code(repo, &diff_files, &[], &[], Some("**/*.py"))
        .expect("filter");

    // glob 絞り込み後は Python ファイルだけが残るべき。
    let names: Vec<String> = files
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.iter().any(|n| n == "bar.py"),
        "py ファイルは glob に一致するため残る。got: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n == "foo.rs"),
        "rs ファイルは glob にマッチしないため除外される。got: {names:?}"
    );
}

/// `filter_dead_by_wip_added` は同一 diff で新規 export されたシンボルを
/// dead から除外する。多段実装中の WIP ノイズ抑止のための既定挙動 (Issue
/// 2026-06-25-wip-dead-symbol-during-incremental-impl 対応)。
#[test]
fn filter_dead_by_wip_added_drops_symbols_listed_in_added() {
    use crate::models::review::{ApiSymbol, DeadSymbol};
    let dead = vec![
        DeadSymbol {
            name: "matchAssigneeName".to_string(),
            kind: "function".to_string(),
            file: "src/notes.ts".to_string(),
            line: None,
        },
        DeadSymbol {
            name: "legacyUnused".to_string(),
            kind: "function".to_string(),
            file: "src/legacy.ts".to_string(),
            line: None,
        },
    ];
    let added = vec![ApiSymbol {
        name: "matchAssigneeName".to_string(),
        kind: "function".to_string(),
        file: "src/notes.ts".to_string(),
        refs_internal: 0,
    }];
    let filtered = filter_dead_by_wip_added(dead, &added);
    let names: Vec<&str> = filtered.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["legacyUnused"],
        "WIP added は dead から除外、既存 dead は残す"
    );
}

/// `filter_dead_by_wip_added` は (file, name) ペアで突合せ、同名でもファイルが
/// 異なれば dead として残す (誤抑制防止)。
#[test]
fn filter_dead_by_wip_added_matches_on_file_and_name_pair() {
    use crate::models::review::{ApiSymbol, DeadSymbol};
    let dead = vec![DeadSymbol {
        name: "helper".to_string(),
        kind: "function".to_string(),
        file: "src/a.ts".to_string(),
        line: None,
    }];
    let added = vec![ApiSymbol {
        // 同じ name だが別 file の追加 — dead 側 (a.ts) は残るべき。
        name: "helper".to_string(),
        kind: "function".to_string(),
        file: "src/b.ts".to_string(),
        refs_internal: 0,
    }];
    let filtered = filter_dead_by_wip_added(dead, &added);
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].file, "src/a.ts");
}

/// `filter_dead_by_wip_added` は added が空なら dead を素通しする。
#[test]
fn filter_dead_by_wip_added_passes_through_when_added_is_empty() {
    use crate::models::review::DeadSymbol;
    let dead = vec![DeadSymbol {
        name: "foo".to_string(),
        kind: "function".to_string(),
        file: "src/foo.rs".to_string(),
        line: None,
    }];
    let filtered = filter_dead_by_wip_added(dead.clone(), &[]);
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].name, "foo");
}
