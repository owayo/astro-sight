use tree_sitter::Node;

use crate::models::location::Range;

use super::{enclosing_of_kind, js_ts_method_name, node_for_symbol_range};

/// Eloquent リレーションの戻り型 (`Illuminate\Database\Eloquent\Relations\*`)。
/// public method の戻り型がこれらのいずれかなら、`->with(['name'])` 文字列リテラルや
/// `$model->name` magic property 経由で Eloquent が呼ぶリレーション定義とみなして
/// dead-code 除外する (GitLab #21)。
const ELOQUENT_RELATION_RETURN_TYPES: &[&str] = &[
    "BelongsTo",
    "BelongsToMany",
    "HasMany",
    "HasOne",
    "HasManyThrough",
    "HasOneThrough",
    "MorphMany",
    "MorphOne",
    "MorphTo",
    "MorphToMany",
    "MorphedByMany",
];

/// Laravel framework が contract 経由で呼ぶ既知の (interface 名, メソッド名) ペア。
/// `implements <interface>` しているクラスでこれらのメソッドは framework runtime が
/// 呼ぶため、static caller 0 件でも dead ではない (GitLab #22)。
///
/// interface 名は単純名 (= `use ... as Foo` の alias) を採用。簡素な実装のため
/// 末尾セグメント比較。
const LARAVEL_CONTRACT_METHODS: &[(&str, &[&str])] = &[
    // Illuminate\Contracts\Auth\CanResetPassword
    (
        "CanResetPassword",
        &["getEmailForPasswordReset", "sendPasswordResetNotification"],
    ),
    // alias で `CanResetPassword as CanResetPasswordContract` と書かれることが多いため両方
    (
        "CanResetPasswordContract",
        &["getEmailForPasswordReset", "sendPasswordResetNotification"],
    ),
];

/// PHP の Laravel runtime entrypoint メソッドを判定する。
///
/// 以下のいずれかなら true:
/// 1. Eloquent リレーション戻り型を持つ public method (`public function x(): BelongsTo`)
///    ― `->with(['x'])` 文字列リテラルや magic property `$model->x` 経由で Eloquent が呼ぶ。
/// 2. Laravel framework が contract 経由で呼ぶ既知のメソッド (`getEmailForPasswordReset` /
///    `sendPasswordResetNotification`) で、enclosing class が対応 contract を implements する。
///
/// 戻り型の判定は末尾名比較 (FQN `\Illuminate\Database\Eloquent\Relations\BelongsTo` でも
/// 末尾の `BelongsTo` でマッチ)。implements 句の判定も末尾名比較で、`use ... as` の alias
/// も両方対応した allowlist で吸収する。
pub fn is_php_laravel_runtime_entrypoint(root: Node, source: &[u8], symbol_range: &Range) -> bool {
    let Some(node) = node_for_symbol_range(root, symbol_range) else {
        return false;
    };
    let mut cur = Some(node);
    let method_node = loop {
        match cur {
            Some(n) if n.kind() == "method_declaration" => break n,
            Some(n) => cur = n.parent(),
            None => return false,
        }
    };

    // (1) Eloquent リレーション戻り型 + public visibility
    if php_method_is_public(method_node, source)
        && php_method_return_type_matches(method_node, source, ELOQUENT_RELATION_RETURN_TYPES)
    {
        return true;
    }

    // (2) Laravel contract 既知メソッド
    let Some(name) = method_node
        .child_by_field_name("name")
        .and_then(|n| n.utf8_text(source).ok())
    else {
        // child_by_field_name が無い場合は child を走査 (PHP は field 名を使わないことが多い)
        return php_method_matches_laravel_contract(method_node, source);
    };
    let _ = name;
    php_method_matches_laravel_contract(method_node, source)
}

/// `method_declaration` ノードの `visibility_modifier` が `public` か判定する。
/// PHP のメソッドは可視性指定が無い場合 `public` 扱いになる慣例のため、
/// visibility_modifier ノードがなければ public とみなす (Laravel Eloquent でも省略多用)。
fn php_method_is_public(method_node: Node, source: &[u8]) -> bool {
    let mut cursor = method_node.walk();
    for child in method_node.children(&mut cursor) {
        if child.kind() == "visibility_modifier" {
            return matches!(child.utf8_text(source).map(|s| s.trim()), Ok("public"));
        }
    }
    true
}

/// `method_declaration` の戻り型ノード (`named_type` / `qualified_name` / `primitive_type` /
/// `union_type` 等) の末尾名が `names` のいずれかにマッチするか判定する。
fn php_method_return_type_matches(method_node: Node, source: &[u8], names: &[&str]) -> bool {
    let mut cursor = method_node.walk();
    let mut after_params = false;
    for child in method_node.children(&mut cursor) {
        if child.kind() == "formal_parameters" {
            after_params = true;
            continue;
        }
        if !after_params {
            continue;
        }
        if matches!(child.kind(), "compound_statement" | ";") {
            break;
        }
        if php_type_node_tail_name_matches(child, source, names) {
            return true;
        }
    }
    false
}

/// PHP の型ノードを再帰的に走査し、末尾の `name` が `names` のいずれかにマッチするか判定する。
/// `named_type` / `qualified_name` / `union_type` / `nullable_type` / `disjunctive_normal_form_type`
/// 等に対応する。
fn php_type_node_tail_name_matches(node: Node, source: &[u8], names: &[&str]) -> bool {
    match node.kind() {
        "name" => node
            .utf8_text(source)
            .ok()
            .is_some_and(|t| names.contains(&t.trim())),
        "qualified_name" => {
            // qualified_name の最後の `name` 子を取る (FQN の末尾セグメント)。
            let mut last_name: Option<Node> = None;
            let mut cursor = node.walk();
            for c in node.children(&mut cursor) {
                if c.kind() == "name" {
                    last_name = Some(c);
                }
            }
            last_name.is_some_and(|n| {
                n.utf8_text(source)
                    .ok()
                    .is_some_and(|t| names.contains(&t.trim()))
            })
        }
        _ => {
            // 型ラッパ (named_type / nullable_type / union_type / intersection_type 等) は
            // 子を再帰的に走査する。
            let mut cursor = node.walk();
            for c in node.children(&mut cursor) {
                if php_type_node_tail_name_matches(c, source, names) {
                    return true;
                }
            }
            false
        }
    }
}

/// `method_declaration` のメソッド名と enclosing class の implements 句を突き合わせ、
/// `LARAVEL_CONTRACT_METHODS` の (interface, method) ペアに一致するか判定する。
fn php_method_matches_laravel_contract(method_node: Node, source: &[u8]) -> bool {
    let Some(name_node) = php_method_name_node(method_node) else {
        return false;
    };
    let Ok(method_name) = name_node.utf8_text(source) else {
        return false;
    };
    let method_name = method_name.trim();

    // どの interface allowlist にメソッド名が含まれるか確認。
    let candidate_interfaces: Vec<&str> = LARAVEL_CONTRACT_METHODS
        .iter()
        .filter_map(|(iface, methods)| {
            if methods.contains(&method_name) {
                Some(*iface)
            } else {
                None
            }
        })
        .collect();
    if candidate_interfaces.is_empty() {
        return false;
    }

    // enclosing class_declaration を探し、implements 句に candidate_interfaces が含まれるか確認。
    let mut cur = method_node;
    while let Some(parent) = cur.parent() {
        if parent.kind() == "class_declaration" {
            return php_class_implements_any(parent, source, &candidate_interfaces);
        }
        cur = parent;
    }
    false
}

/// `method_declaration` の名前ノードを取り出す。
/// tree-sitter-php では field 名 `name` は使われないため child を走査する。
fn php_method_name_node(method_node: Node) -> Option<Node> {
    if let Some(n) = method_node.child_by_field_name("name") {
        return Some(n);
    }
    let mut cursor = method_node.walk();
    let mut last_name: Option<Node> = None;
    for child in method_node.children(&mut cursor) {
        if child.kind() == "name" {
            last_name = Some(child);
            // formal_parameters より前の最初の name がメソッド名。
            break;
        }
    }
    last_name
}

/// PHP の `class_declaration` ノードの implements 句に `interfaces` のいずれかが含まれるか判定する。
/// tree-sitter-php では `class_interface_clause > name` で interface 名が並ぶ。
fn php_class_implements_any(class_node: Node, source: &[u8], interfaces: &[&str]) -> bool {
    let mut cursor = class_node.walk();
    for child in class_node.children(&mut cursor) {
        if child.kind() != "class_interface_clause" {
            continue;
        }
        let mut inner = child.walk();
        for grand in child.children(&mut inner) {
            match grand.kind() {
                "name" => {
                    if let Ok(t) = grand.utf8_text(source)
                        && interfaces.contains(&t.trim())
                    {
                        return true;
                    }
                }
                "qualified_name" => {
                    // FQN の末尾 name を比較
                    let mut last_name: Option<Node> = None;
                    let mut gc = grand.walk();
                    for c in grand.children(&mut gc) {
                        if c.kind() == "name" {
                            last_name = Some(c);
                        }
                    }
                    if let Some(n) = last_name
                        && let Ok(t) = n.utf8_text(source)
                        && interfaces.contains(&t.trim())
                    {
                        return true;
                    }
                }
                _ => {}
            }
        }
    }
    false
}

/// Java の Flyway Java マイグレーションクラスを判定する。
///
/// 判定基準: クラスの **直接の親型 (extends 句の 1 つ目 / implements 句のそれぞれ)** が
/// 以下のどちらかに一致するとき true を返す:
///
/// - 単純名: `BaseJavaMigration` / `JavaMigration` (import 経由のローカル参照)
/// - 完全修飾名: `org.flywaydb.core.api.migration.BaseJavaMigration` /
///   `org.flywaydb.core.api.migration.JavaMigration` (import なしの直接参照)
///
/// `extends com.example.BaseJavaMigration` のような別 package の同名クラスは
/// 完全修飾名が一致しないため false (codex 指摘)。`implements Wrapper<JavaMigration>` の
/// ような generic 型引数も「直接の親型」ではないため false (型引数を再帰走査しない)。
///
/// Flyway はクラスパス走査 + リフレクションで migration を発見・実行するため、
/// アプリコード上の直接参照は存在せず、dead-code / API 変更検出の両方で false positive
/// 源になる。`db/migration/V*__*.java` 命名規約は補助的シグナルだが、ここでは継承関係を
/// 主な判定基準にする (命名だけで除外すると同名の業務クラスを巻き込みやすい)。
/// GitLab issue #24 対応。
pub fn is_java_flyway_migration_class(root: Node, source: &[u8], symbol_range: &Range) -> bool {
    const FLYWAY_SIMPLE: &[&str] = &["BaseJavaMigration", "JavaMigration"];
    const FLYWAY_FQN: &[&str] = &[
        "org.flywaydb.core.api.migration.BaseJavaMigration",
        "org.flywaydb.core.api.migration.JavaMigration",
    ];

    let Some(node) = node_for_symbol_range(root, symbol_range) else {
        return false;
    };

    // class_declaration ノードを探す (symbol_range 内 or 祖先)。
    let mut cur = Some(node);
    let class_node = loop {
        match cur {
            Some(n) if n.kind() == "class_declaration" => break n,
            Some(n) => cur = n.parent(),
            None => return false,
        }
    };

    // superclass (`extends X`) と super_interfaces (`implements X, Y`) を確認。
    // 直接の親型ノードだけ (generic_type の base type、型引数は見ない) を集めて判定する。
    let mut cursor = class_node.walk();
    for child in class_node.children(&mut cursor) {
        match child.kind() {
            "superclass" => {
                if java_direct_supertype_matches(child, source, FLYWAY_SIMPLE, FLYWAY_FQN) {
                    return true;
                }
            }
            "super_interfaces" => {
                // tree-sitter-java の `super_interfaces` は `implements` + `type_list` で構成
                // される (`interface_type_list` ではない)。`implements A, B, C` は `type_list`
                // の各子要素が直接の親型。
                let mut inner = child.walk();
                for grand in child.children(&mut inner) {
                    if grand.kind() == "type_list"
                        && java_direct_supertype_matches(grand, source, FLYWAY_SIMPLE, FLYWAY_FQN)
                    {
                        return true;
                    }
                }
            }
            _ => {}
        }
    }
    false
}

/// 親型節 (`superclass` / `interface_type_list`) の **直接の子要素** から「直接の親型」を
/// 取り出し、Flyway の simple / FQN いずれかに一致するか判定する。型引数 (`generic_type`
/// の `type_arguments`) は親型ではないため見ない。
///
/// 直接の親型ノードは次のいずれか:
///   - `type_identifier`: 単純名 (例: `BaseJavaMigration`)
///   - `scoped_type_identifier`: 完全修飾名 (例: `org.flywaydb.core.api.migration.BaseJavaMigration`)
///   - `generic_type`: ジェネリクス付き親型。base type 部分のみ評価。
fn java_direct_supertype_matches(
    container: Node,
    source: &[u8],
    simple_names: &[&str],
    fqn_names: &[&str],
) -> bool {
    let mut cursor = container.walk();
    for child in container.children(&mut cursor) {
        if java_type_node_matches(child, source, simple_names, fqn_names) {
            return true;
        }
    }
    false
}

/// 「型を表すノード」が指定の simple / FQN にマッチするか判定する。
/// `generic_type` は最初の子 (= base type) を辿って評価する。
/// `scoped_type_identifier` は AST 上の識別子セグメントを `.` で連結したものを比較
/// (raw text ではなく構造ベース) — qualified name の途中に改行・コメント・空白が
/// 挟まれた valid Java でも検出漏れしないようにする。
fn java_type_node_matches(
    type_node: Node,
    source: &[u8],
    simple_names: &[&str],
    fqn_names: &[&str],
) -> bool {
    match type_node.kind() {
        "type_identifier" => type_node
            .utf8_text(source)
            .ok()
            .is_some_and(|text| simple_names.contains(&text)),
        "scoped_type_identifier" => {
            // 再帰的に scoped_type_identifier / type_identifier を辿り、識別子セグメントを
            // `.` で連結する。これにより `org.flywaydb...\n BaseJavaMigration` のような
            // 空白入りの valid Java も `org.flywaydb...BaseJavaMigration` として比較できる。
            let mut segments: Vec<&str> = Vec::new();
            if java_collect_scoped_type_segments(type_node, source, &mut segments) {
                let joined = segments.join(".");
                fqn_names.iter().any(|fqn| joined == *fqn)
            } else {
                false
            }
        }
        "generic_type" => {
            // base type は通常先頭の named child。`A<B>` の `A` 部分。
            let mut cursor = type_node.walk();
            for child in type_node.children(&mut cursor) {
                if matches!(child.kind(), "type_identifier" | "scoped_type_identifier") {
                    return java_type_node_matches(child, source, simple_names, fqn_names);
                }
            }
            false
        }
        _ => false,
    }
}

/// `scoped_type_identifier` を再帰的に辿り、各識別子セグメントを `segments` に push する。
/// scoped_type_identifier の構造: `[scoped_type_identifier|scope] "." type_identifier`。
/// 識別子以外の text (`.` トークンや空白) を比較対象から除くために用いる。
fn java_collect_scoped_type_segments<'a>(
    node: Node<'a>,
    source: &'a [u8],
    segments: &mut Vec<&'a str>,
) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "scoped_type_identifier" | "scope" => {
                if !java_collect_scoped_type_segments(child, source, segments) {
                    return false;
                }
            }
            "type_identifier" | "identifier" => match child.utf8_text(source) {
                Ok(text) => segments.push(text),
                Err(_) => return false,
            },
            _ => {}
        }
    }
    !segments.is_empty()
}

/// Python の関数 / メソッド / クラスがフレームワーク登録デコレータ
/// (Typer / Click / FastAPI / Flask / pytest 等) で装飾されているかを判定する。
///
/// `@app.command(...)` のようなデコレータはフレームワーク内部レジストリに関数を
/// 登録するため、識別子レベルの cross-file refs では caller を追跡できず
/// dead-code 判定で誤陽性になる。本判定で装飾されたシンボルは保守的に
/// 「フレームワーク到達可能」と見なし、dead 候補から除外できる。
///
/// 末尾セグメントマッチ (`<obj>.<method>` 形式):
/// - Typer / Click / Discord.py / aiogram: `command`, `callback`, `group`, `event`
/// - FastAPI / Flask HTTP: `get`, `post`, `put`, `delete`, `patch`, `options`,
///   `head`, `route`, `websocket`
/// - FastAPI / Flask lifecycle: `middleware`, `on_event`, `exception_handler`,
///   `before_request`, `after_request`, `errorhandler`, `teardown_request`,
///   `teardown_appcontext`, `context_processor`, `shell_context_processor`,
///   `before_first_request`
/// - Celery / RQ: `task`
/// - pytest: `fixture`, `parametrize`, `usefixtures`
///
/// 単体名マッチ:
/// - Django: `receiver`, `login_required`, `permission_required`,
///   `csrf_exempt`, `cache_page`, `require_GET`, `require_POST`,
///   `require_http_methods`, `require_safe`, `staff_member_required`,
///   `user_passes_test`
///
/// JS/TS で symbol が「フレームワーク DSL 関数の引数オブジェクトのプロパティ
/// (method shorthand または key:function pair)」かを判定する。該当なら
/// dead-code から除外する (Issue 2026-05-14-wxt-defineContentScript-main)。
///
/// 対象 DSL (allowlist):
///   - WXT: `defineContentScript`, `defineBackground`
///   - Vue: `defineComponent`
///   - Vite/Vitest: `defineConfig`
///   - Nuxt: `defineNuxtConfig`
///
/// 検出パターン (method shorthand):
///   ```text
///   call_expression
///     function: identifier "defineContentScript"
///     arguments: arguments
///       object: object
///         method_definition (= main() { ... })  ← symbol
///   ```
///
/// 検出パターン (pair value):
///   ```text
///   call_expression
///     function: identifier "defineComponent"
///     arguments: arguments
///       object: object
///         pair
///           key: property_identifier "setup"
///           value: arrow_function | function_expression  ← symbol
///   ```
///
/// 直接の親チェーンで判定するため、DSL の callback 内部で定義された関数
/// (本来 dead 判定したい helper) は誤って除外しない。
pub fn is_js_ts_framework_dsl_callback(root: Node, source: &[u8], symbol_range: &Range) -> bool {
    const FRAMEWORK_DSL_CALLEES: &[&str] = &[
        "defineContentScript",
        "defineBackground",
        "defineComponent",
        "defineConfig",
        "defineNuxtConfig",
    ];

    let Some(node) = node_for_symbol_range(root, symbol_range) else {
        return false;
    };

    // symbol を含む最も近い function-like ノードを探す。
    let mut cur = node;
    let outer = loop {
        match cur.kind() {
            "method_definition"
            | "arrow_function"
            | "function_expression"
            | "function_declaration" => break cur,
            _ => {}
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => return false,
        }
    };

    // outer の親チェーンを辿って `object → arguments → call_expression` を確認する。
    // method shorthand: method_definition の親が object
    // pair value:        arrow/function_expression の親が pair → object
    let object = match outer.parent() {
        Some(p) if p.kind() == "object" => p,
        Some(p) if p.kind() == "pair" => match p.parent() {
            Some(o) if o.kind() == "object" => o,
            _ => return false,
        },
        _ => return false,
    };
    let Some(arguments) = object.parent().filter(|p| p.kind() == "arguments") else {
        return false;
    };
    let Some(call_expression) = arguments.parent().filter(|p| p.kind() == "call_expression") else {
        return false;
    };
    let Some(callee) = call_expression.child_by_field_name("function") else {
        return false;
    };
    let Ok(text) = callee.utf8_text(source) else {
        return false;
    };
    FRAMEWORK_DSL_CALLEES.contains(&text)
}

/// Angular の class method lifecycle hook 名集合 (Angular v17+ 公式 docs 準拠)。
///
/// これらは Angular ランタイムが change detection サイクル等で自動呼出するため、
/// ユーザコード側に直接の caller が静的解析で見つからないのが正常。
/// `implements OnInit` 等の interface 実装は Angular の呼出規約では不要なため
/// 判定材料にしない (Angular はメソッド名 + class decorator で hook を解決する)。
///
/// `afterNextRender` / `afterEveryRender` は standalone callback API で
/// クラスメソッドの lifecycle hook ではないため対象外。
const ANGULAR_LIFECYCLE_HOOKS: &[&str] = &[
    "ngOnChanges",
    "ngOnInit",
    "ngDoCheck",
    "ngAfterContentInit",
    "ngAfterContentChecked",
    "ngAfterViewInit",
    "ngAfterViewChecked",
    "ngOnDestroy",
];

/// TS/JS の class 宣言ノード種別。`export abstract class` も
/// `abstract_class_declaration` として現れるため両方を対象にする。
const JS_TS_CLASS_DECLARATION_KINDS: &[&str] = &["class_declaration", "abstract_class_declaration"];

/// `symbol_range` の method が Angular `@Component` / `@Directive` 装飾クラスの
/// lifecycle hook かを判定する。
///
/// 判定:
/// 1. メソッド名が [`ANGULAR_LIFECYCLE_HOOKS`] のいずれかに一致
/// 2. enclosing `class_declaration` に `@Component` または `@Directive` decorator が付与されている
///
/// dead-code 検出側で `exclude_framework_entrypoints == true` のとき除外対象に使う想定。
pub fn is_js_ts_angular_lifecycle_hook(root: Node, source: &[u8], symbol_range: &Range) -> bool {
    let Some(node) = node_for_symbol_range(root, symbol_range) else {
        return false;
    };

    // method_definition を探す
    let Some(method_node) = enclosing_of_kind(node, &["method_definition"]) else {
        return false;
    };

    // メソッド名チェック
    let Some(name) = js_ts_method_name(method_node, source) else {
        return false;
    };
    if !ANGULAR_LIFECYCLE_HOOKS.contains(&name) {
        return false;
    }

    // enclosing class_declaration を探し、@Component / @Directive decorator を確認
    let Some(class_node) = enclosing_of_kind(method_node, JS_TS_CLASS_DECLARATION_KINDS) else {
        return false;
    };
    class_has_component_or_directive_decorator(class_node, source)
}

/// `class_declaration` ノードに `@Component` / `@Directive` decorator が付与されているかを判定する。
///
/// tree-sitter-typescript の AST では decorator は class_declaration の sibling として
/// **直前** に並ぶ (export 文の中では export_statement の子)。class_declaration の親を
/// 走査して周辺の decorator ノードを確認する。
fn class_has_component_or_directive_decorator(class_node: Node, source: &[u8]) -> bool {
    const ANGULAR_DECORATORS: &[&str] = &["Component", "Directive"];

    // class_declaration の前方兄弟と export_statement 経由の decorator の両方を見る
    let containers: [Node; 2] = match class_node.parent() {
        Some(parent) => [parent, class_node],
        None => [class_node, class_node],
    };
    for container in &containers {
        let mut cursor = container.walk();
        for child in container.children(&mut cursor) {
            if child.kind() != "decorator" {
                continue;
            }
            // `@Foo(...)` / `@Foo` の `Foo` を取り出し Angular decorator 名と照合する
            if let Some(name) = decorator_call_name(child, source)
                && ANGULAR_DECORATORS.contains(&name.as_str())
            {
                return true;
            }
        }
    }
    false
}

/// Angular の `ControlValueAccessor` 規約メソッド名。`@Component` / `@Directive` で装飾
/// された CVA 実装クラスに含まれるとき、Angular Forms ランタイムが NG_VALUE_ACCESSOR
/// provider 経由で `ngModel` / `formControl` バインド時に呼び出すため、静的 caller が
/// 0 件でも dead ではない (GitLab #20)。`writeValue` / `registerOnChange` /
/// `registerOnTouched` / `setDisabledState` の 4 メソッドを対象とする。
const ANGULAR_CVA_METHODS: &[&str] = &[
    "writeValue",
    "registerOnChange",
    "registerOnTouched",
    "setDisabledState",
];

/// メンバー単位の Angular runtime entrypoint デコレータ名。これらが property / accessor /
/// method に付与されているとき、Angular ランタイムが change detection / event binding /
/// query 解決経由で呼ぶ・読む・書くため、静的 caller が 0 件でも dead ではない (GitLab #23)。
///
/// 対象:
/// - イベント / バインディング: `@HostListener`, `@HostBinding`
/// - input / output: `@Input`, `@Output`
/// - view / content query: `@ViewChild`, `@ViewChildren`, `@ContentChild`, `@ContentChildren`
const ANGULAR_MEMBER_RUNTIME_DECORATORS: &[&str] = &[
    "HostListener",
    "HostBinding",
    "Input",
    "Output",
    "ViewChild",
    "ViewChildren",
    "ContentChild",
    "ContentChildren",
];

/// `symbol_range` が Angular ランタイムから呼び出される member かを判定する。
/// `is_js_ts_angular_lifecycle_hook` の上位互換で、`@Component` / `@Directive` 装飾クラスの
/// 以下の member を `true` とする (静的 caller 0 件でも dead 候補から除外する想定):
///
/// 1. 既存: lifecycle hook 名 (`ngOnInit` 等)
/// 2. (GitLab #20) `ControlValueAccessor` 規約メソッド (`writeValue` / `registerOnChange` /
///    `registerOnTouched` / `setDisabledState`)。CVA を `implements ControlValueAccessor` で
///    宣言しているか、または同じ意味の `NG_VALUE_ACCESSOR` provider を decorator metadata に
///    持つクラスのみ対象。
/// 3. (GitLab #23) member 単位の Angular decorator (`@HostListener` / `@HostBinding` /
///    `@Input` / `@Output` / `@ViewChild` / `@ViewChildren` / `@ContentChild` /
///    `@ContentChildren`) が付与された property / accessor / method。
pub fn is_js_ts_angular_runtime_entrypoint(
    root: Node,
    source: &[u8],
    symbol_range: &Range,
) -> bool {
    if is_js_ts_angular_lifecycle_hook(root, source, symbol_range) {
        return true;
    }
    if is_js_ts_angular_member_decorator_target(root, source, symbol_range) {
        return true;
    }
    if is_js_ts_angular_cva_contract_method(root, source, symbol_range) {
        return true;
    }
    false
}

/// `symbol_range` の member (property / accessor / method) に Angular の member 単位
/// runtime decorator (`@HostListener` 等) が付与されているか判定する。
/// `@Component` / `@Directive` 装飾クラス配下のメンバーのみ対象とする (誤検出回避)。
fn is_js_ts_angular_member_decorator_target(
    root: Node,
    source: &[u8],
    symbol_range: &Range,
) -> bool {
    let Some(node) = node_for_symbol_range(root, symbol_range) else {
        return false;
    };

    // member ノード (method_definition / public_field_definition 等) を探す。
    // tree-sitter-typescript では @Input() x: string などは `public_field_definition`、
    // @HostListener(...) f() {} などは `method_definition` として表現される。
    let member_kinds = [
        "method_definition",
        "public_field_definition",
        "field_definition",
        "property_signature",
    ];
    let Some(member_node) = enclosing_of_kind(node, &member_kinds) else {
        return false;
    };

    // member を囲む `class_declaration` / `abstract_class_declaration` が Angular 装飾されて
    // いるか確認する (export abstract class も abstract_class_declaration になる)。
    let Some(class_node) = enclosing_of_kind(member_node, JS_TS_CLASS_DECLARATION_KINDS) else {
        return false;
    };
    if !class_has_component_or_directive_decorator(class_node, source) {
        return false;
    }

    // member 自身に decorator が付いているか確認。
    // tree-sitter-typescript では member の直前の child として decorator が並ぶ。
    let parent = member_node.parent().unwrap_or(member_node);
    let mut cursor = parent.walk();
    let mut last_decorator_names: Vec<String> = Vec::new();
    for child in parent.children(&mut cursor) {
        if child.kind() == "decorator" {
            if let Some(name) = decorator_call_name(child, source) {
                last_decorator_names.push(name);
            }
        } else if child.id() == member_node.id() {
            if last_decorator_names
                .iter()
                .any(|n| ANGULAR_MEMBER_RUNTIME_DECORATORS.contains(&n.as_str()))
            {
                return true;
            }
            last_decorator_names.clear();
        } else if child.kind() != "comment" {
            // decorator と member の間に挟まったコメントで decorator との対応が
            // 切れないよう、comment は蓄積を維持したまま読み飛ばす。
            last_decorator_names.clear();
        }
    }
    false
}

/// decorator ノードから `@Foo(...)` の `Foo` 部分の識別子名を取り出す。
fn decorator_call_name(decorator: Node, source: &[u8]) -> Option<String> {
    let mut cursor = decorator.walk();
    for child in decorator.children(&mut cursor) {
        match child.kind() {
            "identifier" => {
                if let Ok(t) = child.utf8_text(source) {
                    return Some(t.to_string());
                }
            }
            "call_expression" => {
                if let Some(callee) = child.child_by_field_name("function")
                    && let Ok(t) = callee.utf8_text(source)
                {
                    return Some(t.to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// `symbol_range` の method が Angular CVA 規約メソッド (`writeValue` 等) かつ enclosing
/// クラスが ControlValueAccessor を実装しているか判定する。
///
/// 「実装」の判定:
/// - `implements ControlValueAccessor` (TS interface 実装節)、または
/// - `@Component({providers: [{provide: NG_VALUE_ACCESSOR, ...}]})` のような provider 登録
///   (decorator メタデータ内に `NG_VALUE_ACCESSOR` 識別子が含まれる)
///
/// どちらも構文上の手がかりを使う (cross-file 解析や型推論は行わない)。GitLab #20 対応。
fn is_js_ts_angular_cva_contract_method(root: Node, source: &[u8], symbol_range: &Range) -> bool {
    let Some(node) = node_for_symbol_range(root, symbol_range) else {
        return false;
    };

    // method_definition を探す。
    let Some(method_node) = enclosing_of_kind(node, &["method_definition"]) else {
        return false;
    };
    let Some(name) = js_ts_method_name(method_node, source) else {
        return false;
    };
    if !ANGULAR_CVA_METHODS.contains(&name) {
        return false;
    }

    // enclosing class が `implements ControlValueAccessor` または NG_VALUE_ACCESSOR
    // provider を持つかで判定する。
    //
    // `@Component` / `@Directive` 装飾の有無は **問わない** (GitLab issue #25 対応):
    // 抽象基底クラスに装飾なしで CVA を実装し、具象子クラスが別ファイルで
    // `@Component({ providers: [...NG_VALUE_ACCESSOR...] })` を宣言して `extends` する
    // 構成が広く使われている。装飾を必須にすると基底側 CVA メソッドが dead 判定される。
    //
    // 同名 interface を非 Angular プロジェクトで独自定義し、かつ CVA 4 メソッドを同形で
    // 持つ確率は実用上ゼロと判断 (`ControlValueAccessor` は @angular/forms の専用契約名)。
    // member decorator (`@HostListener` / `@Input` 等) の判定は引き続き `@Component` /
    // `@Directive` 装飾を必須に維持し、誤抑止リスクを限定する。
    let Some(class_node) = enclosing_of_kind(method_node, JS_TS_CLASS_DECLARATION_KINDS) else {
        return false;
    };
    class_implements_control_value_accessor(class_node, source)
        || class_has_ng_value_accessor_provider(class_node, source)
}

/// `class X implements ControlValueAccessor [...]` の implements 節を判定する。
/// `implements` リストに `ControlValueAccessor` 識別子が含まれていれば true。
fn class_implements_control_value_accessor(class_node: Node, source: &[u8]) -> bool {
    let mut cursor = class_node.walk();
    for child in class_node.children(&mut cursor) {
        if child.kind() != "class_heritage" {
            continue;
        }
        let mut inner = child.walk();
        for grand in child.children(&mut inner) {
            if grand.kind() != "implements_clause" {
                continue;
            }
            // implements_clause の中の type 識別子を順次見る。
            let mut g = grand.walk();
            for type_node in grand.children(&mut g) {
                if let Ok(t) = type_node.utf8_text(source)
                    && t.trim() == "ControlValueAccessor"
                {
                    return true;
                }
                // generic_type / scoped 等の場合は子の識別子も走査
                let mut tcur = type_node.walk();
                for tchild in type_node.children(&mut tcur) {
                    if let Ok(t) = tchild.utf8_text(source)
                        && t.trim() == "ControlValueAccessor"
                    {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// `@Component({ providers: [{provide: NG_VALUE_ACCESSOR, ...}] })` のように decorator
/// メタデータ内に `NG_VALUE_ACCESSOR` 識別子が含まれているか判定する。tree-sitter で
/// 識別子トークン単位の出現を見るだけのざっくり判定 (string literal 内には現れないため
/// 誤検出は起こりにくい)。
fn class_has_ng_value_accessor_provider(class_node: Node, source: &[u8]) -> bool {
    let containers: [Node; 2] = match class_node.parent() {
        Some(parent) => [parent, class_node],
        None => [class_node, class_node],
    };
    for container in &containers {
        let mut cursor = container.walk();
        for child in container.children(&mut cursor) {
            if child.kind() != "decorator" {
                continue;
            }
            if node_contains_identifier(child, source, "NG_VALUE_ACCESSOR") {
                return true;
            }
        }
    }
    false
}

/// `node` 配下の任意の identifier ノードが `target` と一致するかを再帰的に探す。
fn node_contains_identifier(node: Node, source: &[u8], target: &str) -> bool {
    if node.kind() == "identifier"
        && let Ok(t) = node.utf8_text(source)
        && t == target
    {
        return true;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if node_contains_identifier(child, source, target) {
            return true;
        }
    }
    false
}

const ANGULAR_PROVIDER_CALLBACK_TOKENS: &[&str] = &["RECAPTCHA_LOADER_OPTIONS"];
const ANGULAR_PROVIDER_CALLBACK_METHODS: &[&str] = &["onBeforeLoad"];

/// Angular DI provider option に埋め込まれた callback method を runtime entrypoint として判定する。
///
/// 例: `providers: [{ provide: RECAPTCHA_LOADER_OPTIONS, useValue: { onBeforeLoad(url) { ... } } }]`
/// の `onBeforeLoad` は ng-recaptcha 側から呼ばれ、TypeScript 上の caller は現れない。
pub fn is_js_ts_angular_provider_option_callback(
    root: Node,
    source: &[u8],
    symbol_range: &Range,
) -> bool {
    let Some(node) = node_for_symbol_range(root, symbol_range) else {
        return false;
    };

    let Some(callback_node) = enclosing_of_kind(
        node,
        &["method_definition", "function_expression", "arrow_function"],
    ) else {
        return false;
    };

    let (callback_name, containing_object) = match callback_node.kind() {
        "method_definition" => {
            let Some(name_node) = callback_node.child_by_field_name("name") else {
                return false;
            };
            let Ok(name) = name_node.utf8_text(source) else {
                return false;
            };
            let Some(object) = callback_node.parent().filter(|p| p.kind() == "object") else {
                return false;
            };
            (name, object)
        }
        "function_expression" | "arrow_function" => {
            let Some(pair) = callback_node.parent().filter(|p| p.kind() == "pair") else {
                return false;
            };
            let Some(key) = pair.child_by_field_name("key") else {
                return false;
            };
            let Ok(name) = key.utf8_text(source) else {
                return false;
            };
            let Some(object) = pair.parent().filter(|p| p.kind() == "object") else {
                return false;
            };
            (name, object)
        }
        _ => return false,
    };

    if !ANGULAR_PROVIDER_CALLBACK_METHODS.contains(&callback_name) {
        return false;
    }

    let Some(provider_object) = angular_provider_object_for_use_value(containing_object, source)
    else {
        return false;
    };
    angular_provider_object_has_token(provider_object, source)
}

fn angular_provider_object_for_use_value<'a>(
    callback_object: Node<'a>,
    source: &[u8],
) -> Option<Node<'a>> {
    let use_value_pair = callback_object.parent().filter(|p| p.kind() == "pair")?;
    if !pair_key_matches(use_value_pair, source, "useValue") {
        return None;
    }
    if use_value_pair
        .child_by_field_name("value")
        .is_none_or(|value| value.id() != callback_object.id())
    {
        return None;
    }
    use_value_pair.parent().filter(|p| p.kind() == "object")
}

fn angular_provider_object_has_token(provider_object: Node, source: &[u8]) -> bool {
    let mut cursor = provider_object.walk();
    for child in provider_object.children(&mut cursor) {
        if child.kind() != "pair" || !pair_key_matches(child, source, "provide") {
            continue;
        }
        if let Some(value) = child.child_by_field_name("value")
            && ANGULAR_PROVIDER_CALLBACK_TOKENS
                .iter()
                .any(|token| node_contains_identifier(value, source, token))
        {
            return true;
        }
    }
    false
}

fn pair_key_matches(pair: Node, source: &[u8], expected: &str) -> bool {
    let Some(key) = pair.child_by_field_name("key") else {
        return false;
    };
    let Ok(raw) = key.utf8_text(source) else {
        return false;
    };
    let key = raw.trim_matches(|c| c == '\'' || c == '"' || c == '`');
    key == expected
}

/// その他: `pytest.mark.<anything>` プレフィックスを pytest test marker として認識。
pub fn has_framework_entrypoint_decorator_python(
    root: Node,
    source: &[u8],
    symbol_range: &Range,
) -> bool {
    let Some(node) = node_for_symbol_range(root, symbol_range) else {
        return false;
    };

    // 関数 / メソッド / クラス定義の祖先を探す
    let mut current = Some(node);
    let mut def_node = None;
    while let Some(n) = current {
        if matches!(n.kind(), "function_definition" | "class_definition") {
            def_node = Some(n);
            break;
        }
        current = n.parent();
    }
    let Some(def) = def_node else {
        return false;
    };

    // 親が decorated_definition でなければデコレータ無し
    let Some(parent) = def.parent() else {
        return false;
    };
    if parent.kind() != "decorated_definition" {
        return false;
    }

    let mut cursor = parent.walk();
    for child in parent.children(&mut cursor) {
        if child.kind() != "decorator" {
            continue;
        }
        if decorator_matches_framework_pattern_python(child, source) {
            return true;
        }
    }
    false
}

/// Python decorator ノードのテキストから先頭式 (call 引数を除く) を取り出し、
/// 既知のフレームワーク登録パターンに一致するかを判定する。
fn decorator_matches_framework_pattern_python(decorator: Node, source: &[u8]) -> bool {
    let Ok(text) = decorator.utf8_text(source) else {
        return false;
    };
    let stripped = text.trim_start_matches('@').trim();
    let head = stripped.split('(').next().unwrap_or(stripped).trim();
    if head.is_empty() {
        return false;
    }

    // pytest.mark.<anything> は pytest test marker として扱う
    if head.starts_with("pytest.mark.") {
        return true;
    }

    const BARE_DECORATORS: &[&str] = &[
        "receiver",
        "login_required",
        "permission_required",
        "csrf_exempt",
        "cache_page",
        "require_GET",
        "require_POST",
        "require_http_methods",
        "require_safe",
        "staff_member_required",
        "user_passes_test",
    ];
    if BARE_DECORATORS.contains(&head) {
        return true;
    }

    let last = head.rsplit('.').next().unwrap_or(head);
    const TAIL_DECORATORS: &[&str] = &[
        // Typer / Click / Discord.py / aiogram
        "command",
        "callback",
        "group",
        "event",
        // FastAPI / Flask HTTP methods
        "get",
        "post",
        "put",
        "delete",
        "patch",
        "options",
        "head",
        "route",
        "websocket",
        // FastAPI / Flask lifecycle
        "middleware",
        "on_event",
        "exception_handler",
        "before_request",
        "after_request",
        "errorhandler",
        "teardown_request",
        "teardown_appcontext",
        "context_processor",
        "shell_context_processor",
        "before_first_request",
        // Celery / RQ
        "task",
        // pytest
        "fixture",
        "parametrize",
        "usefixtures",
    ];
    TAIL_DECORATORS.contains(&last)
}

/// Python の `class_definition` が直接持つ base class 名のリストを返す。
/// 例: `class Foo(Bar, baz.Qux):` → `["Bar", "baz.Qux"]`
///
/// dead-code 判定で同一ファイル内の継承チェーンを fixed-point で解決するための低レベル
/// helper。クロスファイル継承解析や引数 (`metaclass=...`) の解釈は行わない。
pub fn python_class_base_names(root: Node, source: &[u8], symbol_range: &Range) -> Vec<String> {
    let Some(node) = node_for_symbol_range(root, symbol_range) else {
        return Vec::new();
    };

    // class_definition の祖先を探す
    let mut current = Some(node);
    let mut class_node = None;
    while let Some(n) = current {
        if n.kind() == "class_definition" {
            class_node = Some(n);
            break;
        }
        current = n.parent();
    }
    let Some(class_def) = class_node else {
        return Vec::new();
    };

    // superclasses field は argument_list ノードを含む
    let Some(args) = class_def.child_by_field_name("superclasses") else {
        return Vec::new();
    };

    let mut bases = Vec::new();
    let mut cursor = args.walk();
    for child in args.children(&mut cursor) {
        // identifier (`Bar`) / attribute (`unittest.TestCase`) のみを拾う。
        // keyword_argument (`metaclass=ABCMeta`) や generator 等は無視する。
        if matches!(child.kind(), "identifier" | "attribute")
            && let Ok(text) = child.utf8_text(source)
        {
            bases.push(text.to_string());
        }
    }
    bases
}

/// PHP の擬似 enum (Java enum 風 static factory) パターンを判定する。
///
/// 擬似 enum とは次の形のメソッドを指す。Laravel / DDD 系プロジェクトで
/// `AbstractValueObjectString` を継承した値オブジェクトに大量に存在し、識別子レベルの
/// cross-file refs では caller が追跡できない (migration の文字列リテラル / DB 列値 /
/// reflection annotation 経由で利用される) ため dead 判定すると 100+ 件単位の
/// false positive が出る。
///
/// ```php
/// public static function FOO(): self {
///     return new self('FOO');
/// }
/// ```
///
/// 判定条件 (すべて満たす場合のみ true):
/// - PHP のメソッド宣言ノード
/// - `static` 修飾子付き
/// - 戻り値型注釈が `self` または `static`
/// - 本体に `return new self(<string literal>)` または `return new static(<string literal>)`
/// - 文字列引数のテキストがメソッド名と完全一致 (case-sensitive)
///
/// 第 4 引数 `enclosing_extends_value_object` は本関数の責務外の判定で、
/// 呼び出し側が「`extends AbstractValueObject*` のクラス所属」を確認した場合に
/// `true` を渡すと、より厳格な判定としてフィルタを通す (現状は判定本体には使わない
/// が、将来的により厳しい条件を追加する余地として API シグネチャに残す)。
pub fn is_php_pseudo_enum_method(
    root: Node,
    source: &[u8],
    symbol_range: &Range,
    method_name: &str,
) -> bool {
    let Some(node) = node_for_symbol_range(root, symbol_range) else {
        return false;
    };

    // method_declaration 祖先を取る
    let mut decl_node = None;
    let mut current = Some(node);
    while let Some(n) = current {
        if n.kind() == "method_declaration" {
            decl_node = Some(n);
            break;
        }
        current = n.parent();
    }
    let Some(decl) = decl_node else {
        return false;
    };

    // static 修飾子チェック
    if !php_method_has_static_modifier(decl, source) {
        return false;
    }

    // 戻り値型注釈が self または static
    if !php_return_type_is_self_or_static(decl, source) {
        return false;
    }

    // 本体内で `return new self(method_name)` 相当を検出
    let Some(body) = decl.child_by_field_name("body") else {
        return false;
    };
    php_body_returns_new_self_with_name(body, source, method_name)
}

fn php_method_has_static_modifier(decl: Node<'_>, source: &[u8]) -> bool {
    let mut cursor = decl.walk();
    for child in decl.children(&mut cursor) {
        // tree-sitter-php では static 修飾子は `static_modifier` として直接 child に出る。
        if child.kind() == "static_modifier" {
            return true;
        }
        // フォールバック: テキスト走査で `static` キーワードを含むかチェック (古いノード名対応)
        if child.kind() == "visibility_modifier" || child.kind() == "abstract_modifier" {
            continue;
        }
        if let Ok(text) = child.utf8_text(source)
            && text.split_whitespace().any(|tok| tok == "static")
        {
            return true;
        }
    }
    false
}

fn php_return_type_is_self_or_static(decl: Node<'_>, source: &[u8]) -> bool {
    // tree-sitter-php では戻り値型は `return_type` フィールドにある (named_type / primitive_type)
    let Some(rt) = decl.child_by_field_name("return_type") else {
        return false;
    };
    let Ok(text) = rt.utf8_text(source) else {
        return false;
    };
    let trimmed = text.trim();
    matches!(trimmed, "self" | "static")
}

/// 本体に `return new self('NAME')` または `return new static('NAME')` があり、
/// 引数の文字列リテラル内容が `method_name` と一致するかを判定する。
fn php_body_returns_new_self_with_name(body: Node<'_>, source: &[u8], method_name: &str) -> bool {
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        if php_node_returns_new_self_with_name(child, source, method_name) {
            return true;
        }
    }
    false
}

fn php_node_returns_new_self_with_name(node: Node<'_>, source: &[u8], method_name: &str) -> bool {
    // return_statement → object_creation_expression を探す
    if node.kind() == "return_statement" {
        // return の expression を取得
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "object_creation_expression"
                && php_object_creation_matches_name(child, source, method_name)
            {
                return true;
            }
        }
    }
    // 再帰: ネストした control flow (try/match/if) の中も探す
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if php_node_returns_new_self_with_name(child, source, method_name) {
            return true;
        }
    }
    false
}

fn php_object_creation_matches_name(node: Node<'_>, source: &[u8], method_name: &str) -> bool {
    // `new self('NAME')` の type 部分が self / static
    let type_node = node.child_by_field_name("type").or_else(|| {
        // tree-sitter-php のバージョンによっては type フィールドが付かないので
        // 子ノードから直接探す
        let mut cursor = node.walk();
        node.children(&mut cursor).find(|c| {
            matches!(
                c.kind(),
                "name" | "scoped_call_expression" | "qualified_name"
            )
        })
    });
    let type_ok = type_node
        .and_then(|n| n.utf8_text(source).ok())
        .is_some_and(|t| matches!(t.trim(), "self" | "static"));
    if !type_ok {
        return false;
    }

    // 引数リスト (arguments) を取得
    let args = node.child_by_field_name("arguments").or_else(|| {
        let mut cursor = node.walk();
        node.children(&mut cursor).find(|c| c.kind() == "arguments")
    });
    let Some(args) = args else {
        return false;
    };

    // 1 つ目の引数が string literal で、その中身が method_name と一致するかをチェック
    let mut cursor = args.walk();
    for child in args.children(&mut cursor) {
        // argument ノードを skip して中身の string literal を探す
        let actual = if child.kind() == "argument" {
            child.named_child(0)
        } else {
            Some(child)
        };
        let Some(actual) = actual else { continue };
        if matches!(actual.kind(), "string" | "encapsed_string") {
            // string ノードの中身は string_value / string_content / encapsed_string で囲まれる
            let Ok(raw) = actual.utf8_text(source) else {
                continue;
            };
            // クォートを剥がす ('FOO' / "FOO" のいずれも対応)
            let trimmed = raw.trim();
            let stripped = trimmed
                .strip_prefix('\'')
                .and_then(|s| s.strip_suffix('\''))
                .or_else(|| trimmed.strip_prefix('"').and_then(|s| s.strip_suffix('"')));
            if let Some(literal) = stripped
                && literal == method_name
            {
                return true;
            }
            // 1 つ目の引数で結論が出たので break
            return false;
        }
    }
    false
}

/// PHP メソッドの docstring (`/** ... */`) に runtime annotation が含まれるかを判定する。
///
/// `@TypeItem`, `@dataProvider`, `@DataProvider`, `@Route`, `@Listen` 等、reflection
/// 経由でフレームワークから動的に呼ばれることを示すアノテーションが付いていれば true。
/// `Symbol.doc` から渡される文字列をチェックするため AST 走査は不要。
pub fn php_doc_has_runtime_annotation(doc: &str) -> bool {
    // `@\App\Annotations\TypeItem(...)` のような fully qualified 形式と
    // `@TypeItem(...)` のような short 形式の両方に対応するため、`@<Identifier>` を
    // 走査する。識別子の頭文字が大文字 (PHP の attribute / annotation 慣習) を要件にする。
    for token in doc.split('@').skip(1) {
        // token の先頭から識別子部分だけ取り出す
        let name: String = token
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '\\')
            .collect();
        let last = name.rsplit('\\').next().unwrap_or(name.as_str());
        // 大文字始まりの annotation (PSR / Doctrine 慣習) を検出
        if last.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
            // `@TypeItem`, `@DataProvider`, `@Route`, `@Listen`, `@Bean` 等
            return true;
        }
        // 小文字始まりでも reflection 経由でよく使われるものは別途列挙
        const RUNTIME_LOWER_ANNOTATIONS: &[&str] = &[
            "dataProvider",
            "depends",
            "test",
            "before",
            "after",
            "beforeClass",
            "afterClass",
            "covers",
            "uses",
            "group",
            "ticket",
            "preserveGlobalState",
            "runInSeparateProcess",
        ];
        if RUNTIME_LOWER_ANNOTATIONS.contains(&last) {
            return true;
        }
    }
    false
}
