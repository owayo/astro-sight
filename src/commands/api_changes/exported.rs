//! 公開 API 面の抽出とフィルタ。定義・非 API 要素・実行時入口・qualname 規約による除外を担う。

use super::*;

/// qualname (`Container.method`) から末尾セグメントのみを抜き出す。
/// `a.b.c` → `c`、`foo` → `foo`。
pub(crate) fn bare_name(qualname: &str) -> &str {
    qualname.rsplit('.').next().unwrap_or(qualname)
}

/// ファイルリストからエクスポートシンボルを収集し、参照ゼロのシンボルを返す。
/// dead-code コマンドと review コマンドの共通コアロジック。
/// count_non_definition_refs_split で production / test 別に件数のみカウントし、
/// SymbolReference を確保しない。
pub(crate) fn extract_exported_symbols_from_git(
    dir: &str,
    base: &str,
    file_path: &str,
) -> Option<Vec<(String, String, String)>> {
    // テストファイル配下のシンボルは API 差分検出の対象外。
    // (api.rm の base 側比較もテストファイルからは行わない)
    if is_test_path(std::path::Path::new(file_path)) {
        return Some(Vec::new());
    }
    // revision / path の検証は git_show_blob 側で強制される
    let old_source = git_show_blob(dir, base, file_path)?;
    extract_exported_symbols_from_source(file_path, &old_source)
}

/// 与えられた旧側ソースから export シンボル一覧を抽出する。
///
/// `extract_exported_symbols_from_git` のフォールバックとして、`--diff-file` の削除 hunk から
/// 復元した旧ソースを直接渡す経路で使う。test path 判定とフィルタは git 経路と同一。
pub(crate) fn extract_exported_symbols_from_source(
    file_path: &str,
    source: &[u8],
) -> Option<ExportedSymbols> {
    if is_test_path(std::path::Path::new(file_path)) {
        return Some(Vec::new());
    }
    let utf8_path = camino::Utf8Path::new(file_path);
    let lang_id = parser::detect_lang(utf8_path, source).ok()?;
    let tree = parser::parse_source(source, lang_id).ok()?;
    let root = tree.root_node();

    let syms = crate::engine::symbols::extract_symbols(root, source, lang_id).ok()?;
    // Rust の `impl Trait for Type` 配下のメソッドは trait の実装事実であり、独立した
    // 公開 API item ではない。module 移動など実体は維持したままの変更でも api.add / api.rm
    // に誤計上されるのを避けるため、API 変更検出でも trait impl メソッドを除外する。
    // 旧側を読む経路は API 変更検出 (api.rm 比較) のみで使われる。
    // dead-code は最新コミット側だけを見るため framework entrypoint の除外は不要。
    Some(filter_exported_symbols(
        &syms,
        root,
        source,
        lang_id,
        true,
        false,
        Some(file_path),
    ))
}

#[cfg(test)]
pub(crate) fn extract_exported_symbols_from_file_inner(
    dir: &str,
    file_path: &str,
    exclude_trait_impls: bool,
    exclude_framework_entrypoints: bool,
) -> Option<ExportedSymbols> {
    extract_exported_symbols_from_file_inner_with_lang(
        dir,
        file_path,
        exclude_trait_impls,
        exclude_framework_entrypoints,
    )
    .map(|(_, syms)| syms)
}

pub(crate) fn extract_exported_symbols_from_file_inner_with_lang(
    dir: &str,
    file_path: &str,
    exclude_trait_impls: bool,
    exclude_framework_entrypoints: bool,
) -> Option<ExportedSymbolsWithLang> {
    // diff から得た file_path は信頼境界外。`../etc/passwd` 等のトラバーサルや絶対パスを
    // 拒否し、workspace 外のファイルを誤って読まないようにする。
    if !crate::engine::impact::is_safe_diff_path(file_path) {
        return None;
    }
    let full_path = std::path::Path::new(dir).join(file_path);
    let utf8_path = camino::Utf8Path::new(full_path.to_str()?);
    let source = parser::read_file(utf8_path).ok()?;
    let lang_id = parser::detect_lang(utf8_path, &source).ok()?;

    // lexer-only 言語 (現状 Xojo) は tree-sitter を持たないため、lexer 経由で
    // export 相当のシンボルを抽出する。
    if let crate::language::DetectedLang::LexerOnly(lexer_lang) = lang_id.detected() {
        return Some((
            lang_id,
            crate::engine::lexer::extract_exported_symbols(
                &source,
                lexer_lang,
                exclude_framework_entrypoints,
            ),
        ));
    }

    let tree = parser::parse_source(&source, lang_id).ok()?;
    let root = tree.root_node();

    let syms = crate::engine::symbols::extract_symbols(root, &source, lang_id).ok()?;
    Some((
        lang_id,
        filter_exported_symbols(
            &syms,
            root,
            &source,
            lang_id,
            exclude_trait_impls,
            exclude_framework_entrypoints,
            Some(file_path),
        ),
    ))
}

/// 公開面の判定中にファイル単位で共有する不変情報。
///
/// 各除外規則はこの情報と1シンボルだけを参照する純粋な述語として分離する。
/// 規則ごとに異なる `exclude_framework_entrypoints` の適用条件は、過去の誤検出を
/// 防ぐため一律化せず個別に保持する。
pub(crate) struct ExportSurfaceContext<'tree, 'source> {
    root: tree_sitter::Node<'tree>,
    source: &'source [u8],
    lines: Vec<&'source str>,
    lang_id: crate::language::LangId,
    exclude_trait_impls: bool,
    exclude_framework_entrypoints: bool,
    file_path: Option<&'source str>,
    containers: Vec<&'source Symbol>,
    unittest_classes: HashSet<String>,
}

impl<'tree, 'source> ExportSurfaceContext<'tree, 'source> {
    pub(crate) fn new(
        syms: &'source [Symbol],
        root: tree_sitter::Node<'tree>,
        source: &'source [u8],
        lang_id: crate::language::LangId,
        exclude_trait_impls: bool,
        exclude_framework_entrypoints: bool,
        file_path: Option<&'source str>,
    ) -> Self {
        let source_str = std::str::from_utf8(source).unwrap_or("");
        let lines = source_str.lines().collect();

        // 同名別メソッドを区別するため、class/struct/trait/interface/enum を収集する。
        // メソッド/関数の範囲を内包する最も内側の要素を qualname に使う。
        let containers = syms
            .iter()
            .filter(|sym| {
                matches!(
                    sym.kind,
                    SymbolKind::Class
                        | SymbolKind::Struct
                        | SymbolKind::Trait
                        | SymbolKind::Interface
                        | SymbolKind::Enum
                )
            })
            .collect();

        // Python 限定: 同一ファイル内の `unittest.TestCase` 派生クラスを固定点計算で解決する。
        // dead-code 経路だけで使うため、実行時入口を除外する場合に限って構築する。
        let unittest_classes =
            if exclude_framework_entrypoints && lang_id == crate::language::LangId::Python {
                collect_python_unittest_classes(syms, root, source, lang_id)
            } else {
                HashSet::new()
            };

        Self {
            root,
            source,
            lines,
            lang_id,
            exclude_trait_impls,
            exclude_framework_entrypoints,
            file_path,
            containers,
            unittest_classes,
        }
    }

    fn is_excluded_before_qualname(&self, sym: &Symbol) -> bool {
        self.is_non_definition(sym) || self.is_non_api_item(sym) || self.is_runtime_entrypoint(sym)
    }

    /// Rust: メソッドのレシーバ型がクレート外から到達できない場合、`pub fn` でも
    /// 外部公開 API ではないと判定する (実効可視性 = min(宣言, 所有型, モジュール))。
    ///
    /// `pub(crate) struct Holder` の inherent impl 内 `pub fn value()` はクレート外から
    /// `Holder` に到達できない以上呼べないが、宣言の `pub` だけを見ていたため
    /// 内部リファクタのたびに blocking な `api.mod` が出て、本当に外部 API を壊した
    /// ときの信号が埋もれていた。
    ///
    /// レシーバ型の宣言が同一ファイルに無い (別ファイルの型への inherent impl)、
    /// 同名の型宣言が複数ある、trait impl である場合は fail-closed で公開扱いを維持する。
    /// モジュール階層と `pub use` 経路の到達性は既存の `rust_public.rs` が別途見る。
    fn rust_owner_type_is_crate_internal(&self, sym: &Symbol, qualname: &str) -> bool {
        if self.lang_id != crate::language::LangId::Rust {
            return false;
        }
        if !matches!(sym.kind, SymbolKind::Method | SymbolKind::Function) {
            return false;
        }
        // bare name (トップレベル関数) はレシーバ型を持たない
        let Some((owner, _)) = qualname.rsplit_once('.') else {
            return false;
        };

        // 型宣言だけを候補にする。`impl Holder` ブロック自体も同名の container として
        // 収集されるが (kind = Type)、`impl` 行に `pub` は書けないため候補に混ぜると
        // すべての inherent impl メソッドが内部扱いに落ちる (過剰抑制)。
        let mut decls = self.containers.iter().filter(|c| {
            c.name == owner
                && matches!(
                    c.kind,
                    SymbolKind::Struct | SymbolKind::Enum | SymbolKind::Class
                )
        });
        let Some(owner_decl) = decls.next() else {
            return false; // 別ファイルの型 → fail-closed
        };
        if decls.next().is_some() {
            return false; // 同名の型宣言が複数 → fail-closed
        }

        // 可視性は宣言行のテキストではなく AST の `visibility_modifier` で判定する
        // (`pub\nstruct Holder;` や `pub /* c */ struct Holder;` を crate 内部と
        // 誤判定すると、公開 API の変更を取りこぼす false negative になる)。
        // 判定不能なら fail-closed で公開扱いを維持する。
        match crate::engine::symbols::is_rust_declaration_unrestricted_pub(
            self.root,
            self.source,
            &owner_decl.range,
        ) {
            Some(unrestricted_pub) => !unrestricted_pub,
            None => false,
        }
    }

    /// 定義または公開シンボルとして扱えない要素を除外する。
    fn is_non_definition(&self, sym: &Symbol) -> bool {
        // モジュール宣言 (`pub mod foo;`) はファイル構成の整理であり、
        // 公開 API 面としての意味は薄い。dead-code / api.add 両経路で除外する
        // (Rust `mod`, Python の module、他言語の同等表現)。
        if matches!(sym.kind, SymbolKind::Module) {
            return true;
        }
        if !crate::engine::symbols::is_symbol_exported(
            self.root,
            self.source,
            self.lang_id,
            &sym.range,
        ) {
            return true;
        }
        // pub(crate), pub(super) 等はクレート内部 API なので除外。
        //
        // 可視性は宣言行のテキストではなく AST の `visibility_modifier` で判定する
        // (`rust_owner_type_is_crate_internal` と同じ規約)。旧実装は宣言行に `"pub("` が
        // 含まれるかの**部分一致**だったため、`pub struct S { pub(crate) a: u32 }` のように
        // フィールドだけが制限付きの公開型や、`pub fn to_epub()` のように名前がたまたま
        // `pub(` を含む関数まで公開 API 面から消えていた
        // (api.add / api.rm / api.mod / dead-code の 4 経路が同時に沈黙する)。
        // 判定不能なら fail-closed で公開扱いを維持する。
        if self.lang_id == crate::language::LangId::Rust
            && crate::engine::symbols::is_rust_declaration_restricted_pub(
                self.root,
                self.source,
                &sym.range,
            )
            .unwrap_or(false)
        {
            return true;
        }
        // C/C++ で実関数 body 内にネストした function_definition は、tree-sitter-cpp が
        // マクロ呼び出し (BOOST_FOREACH 等) を関数定義と誤パースした結果であることが多い。
        // 本物のトップレベル関数 / クラスメソッドではないため dead-code / API 変更検出の
        // どちらでも exported シンボルから除外する
        // (Issue #13: api_changes.modified が差分外の BOOST_FOREACH を拾う誤検出対策)。
        if matches!(
            self.lang_id,
            crate::language::LangId::C | crate::language::LangId::Cpp
        ) && matches!(sym.kind, SymbolKind::Function | SymbolKind::Method)
            && crate::engine::symbols::is_cpp_nested_function(self.root, &sym.range)
        {
            return true;
        }
        // C/C++ の前方宣言・opaque tag (本体を持たない struct/class/enum) は「定義」ではなく
        // 宣言であり、dead-code (未使用定義検出) や API 変更の対象にすべきではない。
        // `typedef struct st_mysql MYSQL;` の st_mysql (外部ライブラリの不透明構造体タグ) を
        // dead 誤検出する問題への対応 (Issue #11)。
        if matches!(
            self.lang_id,
            crate::language::LangId::C | crate::language::LangId::Cpp
        ) && matches!(
            sym.kind,
            SymbolKind::Struct | SymbolKind::Class | SymbolKind::Enum
        ) && crate::engine::symbols::is_cpp_forward_declaration(self.root, &sym.range)
        {
            return true;
        }

        false
    }

    /// 公開されていても独立したAPI要素として扱わないメンバーを除外する。
    fn is_non_api_item(&self, sym: &Symbol) -> bool {
        // Rust の `impl Trait for Type` 配下のメソッドは除外する。
        //   - dead-code 判定: trait dispatch 経由で呼ばれるため cross-file refs で caller を
        //     追跡できず、偽陽性になる。
        //   - API 変更検出: trait メソッドの実装は公開 item ではなく実装事実のため、個別の
        //     `on_ref` / `default` 等を api.add / api.rm にしない。必要であれば `impl Trait
        //     for Type` 単位で差分を扱うべきで、メソッド単位では扱わない。
        if self.exclude_trait_impls
            && self.lang_id == crate::language::LangId::Rust
            && crate::engine::symbols::is_trait_impl_method_rust(self.root, &sym.range)
        {
            return true;
        }
        // Kotlin/Java/Swift/TS/C# の `override` メソッドは親 interface/class の
        // メソッドを実装しているため、親型経由（Android の Listener callback 等）
        // で呼ばれる。cross-file refs では caller を追跡できず dead-code / api.add/rm
        // のいずれでも偽陽性になるため除外する。
        if self.exclude_trait_impls
            && matches!(sym.kind, SymbolKind::Method | SymbolKind::Function)
            && crate::engine::symbols::is_override_method(
                self.root,
                self.source,
                self.lang_id,
                &sym.range,
            )
        {
            return true;
        }
        // TS/JS の `constructor` メソッドは `new ClassName(...)` 構文で暗黙的に呼び出される。
        // 識別子レベルの cross-file refs では `constructor` 名を探しても見つからず、
        // クラスが利用されていても dead 判定される。クラス自体の dead 判定で十分なので、
        // constructor を独立した API/dead 候補から除外する。
        if matches!(sym.kind, SymbolKind::Method)
            && sym.name == "constructor"
            && matches!(
                self.lang_id,
                crate::language::LangId::Typescript
                    | crate::language::LangId::Tsx
                    | crate::language::LangId::Javascript
            )
        {
            return true;
        }

        false
    }

    /// フレームワークやテストランナーが動的に呼ぶ実行時入口を除外する。
    ///
    /// この関数の入口で `exclude_framework_entrypoints` を一律適用してはいけない。
    /// PHPUnit・TS/JS constructor・Flyway はAPI差分でも除外する一方、Laravelなどは
    /// dead-code 経路だけで除外する必要がある。
    fn is_runtime_entrypoint(&self, sym: &Symbol) -> bool {
        // PHPUnit 規約のテストメソッド / テストクラス。PHP 限定。
        // `public function testXxx`, `setUp`, `tearDown`, `setUpBeforeClass`,
        // `tearDownAfterClass`, および `*Test` / `*TestCase` / `*IntegrationTest` /
        // `*FeatureTest` クラスは PHPUnit のランナーから自動で呼ばれる規約的シンボルで、
        // 識別子レベルの cross-file ref は発生しないが dead でもない。
        if is_phpunit_test_symbol(&sym.name, sym.kind, self.lang_id) {
            return true;
        }
        // PHP 擬似 enum (Java enum 風 static factory) パターン。PHP 限定。
        // `public static function FOO(): self { return new self('FOO'); }` 形式は
        // Laravel / DDD 系の AbstractValueObject 系で大量に存在し、
        // migration の文字列リテラル / DB 列値 / annotation reflection 経由で
        // 利用されるが識別子レベルの cross-file refs では caller が追跡できない。
        // dead-code の framework_entrypoints 除外と同じ意味合いで除外する。
        if self.exclude_framework_entrypoints
            && self.lang_id == crate::language::LangId::Php
            && matches!(sym.kind, SymbolKind::Method)
            && crate::engine::symbols::is_php_pseudo_enum_method(
                self.root,
                self.source,
                &sym.range,
                &sym.name,
            )
        {
            return true;
        }
        // PHP の runtime annotation (`@TypeItem`, `@Route`, `@DataProvider`, `@dataProvider` 等) が
        // docstring に付いているメソッド / クラスは reflection 経由で動的に呼ばれるため
        // dead-code 候補から除外する。
        if self.exclude_framework_entrypoints
            && self.lang_id == crate::language::LangId::Php
            && matches!(sym.kind, SymbolKind::Method | SymbolKind::Class)
            && let Some(doc) = sym.doc.as_deref()
            && crate::engine::symbols::php_doc_has_runtime_annotation(doc)
        {
            return true;
        }
        // Python のフレームワーク登録デコレータ (Typer / Click / FastAPI / Flask /
        // pytest 等) で装飾された関数 / メソッド / クラスは、フレームワーク内部
        // レジストリ経由で呼び出されるため識別子レベルの cross-file refs では
        // caller を追跡できない。dead-code 判定では偽陽性源になるため除外する。
        if self.exclude_framework_entrypoints
            && self.lang_id == crate::language::LangId::Python
            && matches!(
                sym.kind,
                SymbolKind::Method | SymbolKind::Function | SymbolKind::Class
            )
            && crate::engine::symbols::has_framework_entrypoint_decorator_python(
                self.root,
                self.source,
                &sym.range,
            )
        {
            return true;
        }
        // JS/TS のフレームワーク DSL コールバック (WXT defineContentScript /
        // defineBackground、Vue defineComponent、Vite/Nuxt defineConfig 等) の
        // 引数オブジェクトメソッド (`main()`, `setup()` 等) は、フレームワーク内部
        // からビルド時連結で呼び出されるため識別子レベルの cross-file refs では
        // caller を追跡できない (Issue 2026-05-14-wxt-defineContentScript-main)。
        if self.exclude_framework_entrypoints
            && matches!(
                self.lang_id,
                crate::language::LangId::Typescript
                    | crate::language::LangId::Tsx
                    | crate::language::LangId::Javascript
            )
            && matches!(sym.kind, SymbolKind::Method | SymbolKind::Function)
            && crate::engine::symbols::is_js_ts_framework_dsl_callback(
                self.root,
                self.source,
                &sym.range,
            )
        {
            return true;
        }
        // Angular DI provider option callback。例: RECAPTCHA_LOADER_OPTIONS の
        // `useValue: { onBeforeLoad() { ... } }` はライブラリ側から呼ばれるため、
        // TS 上の直接 caller が無くても dead ではない (GitLab #26)。
        if self.exclude_framework_entrypoints
            && matches!(
                self.lang_id,
                crate::language::LangId::Typescript
                    | crate::language::LangId::Tsx
                    | crate::language::LangId::Javascript
            )
            && matches!(sym.kind, SymbolKind::Method | SymbolKind::Function)
            && crate::engine::symbols::is_js_ts_angular_provider_option_callback(
                self.root,
                self.source,
                &sym.range,
            )
        {
            return true;
        }
        // Angular `@Component` / `@Directive` 装飾クラスの runtime entrypoint メンバー。
        // 以下の 3 系統を統合判定する (詳細は is_js_ts_angular_runtime_entrypoint):
        //   1. lifecycle hook メソッド (`ngOnInit` / `ngAfterViewChecked` 等、既存)
        //      Angular ランタイムが change detection サイクルで自動呼出するため静的 caller が無い。
        //      GitLab issue #8 対応。
        //   2. ControlValueAccessor 規約メソッド (`writeValue` / `registerOnChange` /
        //      `registerOnTouched` / `setDisabledState`)。`implements ControlValueAccessor` または
        //      decorator metadata 内の `NG_VALUE_ACCESSOR` provider をシグナルとして判定。
        //      Angular Forms が NG_VALUE_ACCESSOR provider 経由で ngModel/formControl バインド時に
        //      呼ぶ。GitLab issue #20 対応。
        //   3. member 単位の Angular decorator (`@HostListener` / `@HostBinding` / `@Input` /
        //      `@Output` / `@ViewChild` / `@ViewChildren` / `@ContentChild` / `@ContentChildren`)
        //      が付与された method/property。GitLab issue #23 対応。
        if self.exclude_framework_entrypoints
            && matches!(
                self.lang_id,
                crate::language::LangId::Typescript
                    | crate::language::LangId::Tsx
                    | crate::language::LangId::Javascript
            )
            && matches!(sym.kind, SymbolKind::Method | SymbolKind::Field)
            && crate::engine::symbols::is_js_ts_angular_runtime_entrypoint(
                self.root,
                self.source,
                &sym.range,
            )
        {
            return true;
        }
        // Laravel runtime entrypoint。PHP 限定。dead-code 経路のみ
        // (`exclude_framework_entrypoints=true`) で除外する。
        // 以下の 2 系統:
        //   1. Eloquent リレーション (`public function x(): BelongsTo` 等の戻り型)。`->with('x')`
        //      文字列リテラルや `$model->x` magic property 経由で Eloquent が呼ぶため、
        //      static caller 0 件でも dead ではない (GitLab issue #21)。
        //   2. Laravel framework が contract 経由で呼ぶ既知のメソッド名 (`getEmailForPasswordReset`
        //      / `sendPasswordResetNotification`)。enclosing class が `CanResetPassword(Contract)?`
        //      を `implements` する場合のみ対象 (GitLab issue #22)。
        // 文字列リテラル参照 (`with(['x'])`) / magic property 解決は静的解析の本質的限界のため
        // 別 issue としている (codex 設計判断)。
        //
        // API 差分経路 (`exclude_framework_entrypoints=false`) では除外しない。判定が戻り型
        // ベースのため、戻り型なし旧版 (`function x() {`) は残り戻り型付き新版 (`(): HasOne`)
        // だけ除外される非対称が起き、実在メソッドの返り型付与が api.rm に誤分類されていた
        // (GitLab issue #33)。公開メソッドである以上シグネチャ差分は api.mod として扱う。
        if self.exclude_framework_entrypoints
            && self.lang_id == crate::language::LangId::Php
            && matches!(sym.kind, SymbolKind::Method | SymbolKind::Function)
            && crate::engine::symbols::is_php_laravel_runtime_entrypoint(
                self.root,
                self.source,
                &sym.range,
            )
        {
            return true;
        }
        // Flyway の Java マイグレーションクラスとそのメンバ。Java 限定。
        // `extends BaseJavaMigration` / `implements JavaMigration` のクラスは Flyway が
        // クラスパス走査 + リフレクションで発見・実行するため、アプリコード上に直接参照が
        // 存在せず dead-code / API 変更検出の両方で false positive 源になる。クラス自体に
        // 加えて配下の `migrate(Context)` 等のメソッドも framework が反射で呼ぶため除外する
        // (sym.range から class_declaration 祖先まで遡って判定)。Java symbols 抽出では
        // クラスメソッドが `SymbolKind::Function` で返る (Method ではない) ため Function/Method
        // の両方を許容する。本判定は `exclude_framework_entrypoints` フラグに依存させず常に
        // 効かせる ― API 変更検出の new 側 (`extract_new_file_facts`) / old 側
        // (`extract_old_exported_symbols`) は flag=false で呼ばれるが、Flyway migration は
        // 公開 API 面ではない runtime entrypoint なので api.added / api.removed にも出さない
        // ため。GitLab issue #24 対応。
        if self.lang_id == crate::language::LangId::Java
            && matches!(
                sym.kind,
                SymbolKind::Class | SymbolKind::Method | SymbolKind::Function
            )
            && crate::engine::symbols::is_java_flyway_migration_class(
                self.root,
                self.source,
                &sym.range,
            )
        {
            return true;
        }
        // unittest / pytest のテスト規約シンボル。Python 限定。
        // `class Foo(unittest.TestCase):` 派生クラスとそのメソッド (`test_*`,
        // `setUp` 等)、`test_*.py` / `*_test.py` のトップレベル `test_*` 関数、
        // `conftest.py` 内の関数はテストランナーから動的 discover されるため、
        // 識別子レベルの cross-file refs では caller を追跡できない。
        if self.exclude_framework_entrypoints
            && self.lang_id == crate::language::LangId::Python
            && matches!(sym.kind, SymbolKind::Method | SymbolKind::Function)
            && crate::engine::symbols::is_python_dynamic_protocol_method(
                self.root,
                self.source,
                &sym.range,
                &sym.name,
            )
        {
            return true;
        }

        if self.exclude_framework_entrypoints
            && is_python_test_symbol(
                &sym.name,
                sym.kind,
                self.lang_id,
                self.file_path,
                sym.container.as_deref(),
                &self.unittest_classes,
            )
        {
            return true;
        }

        false
    }

    fn qualname(&self, sym: &Symbol) -> String {
        if matches!(sym.kind, SymbolKind::Method | SymbolKind::Function) {
            // 抽出器が container を確定させている場合はそれを優先する。
            // Go のメソッドはレシーバ型が container だが宣言はトップレベルに並ぶため、
            // range 内包ベースの `enclosing_container` では引き当てられない。
            if let Some(container) = sym.container.as_deref() {
                return format!("{container}.{}", sym.name);
            }
            enclosing_container(sym, &self.containers)
                .map(|c| format!("{}.{}", c.name, sym.name))
                .unwrap_or_else(|| sym.name.clone())
        } else {
            sym.name.clone()
        }
    }

    /// qualname の確定後にだけ評価できる規約除外を判定する。
    fn is_excluded_by_qualname(&self, sym: &Symbol, qualname: &str) -> bool {
        // qualname ベースでも最終チェック (例: `Foo.testBar` を PHP で除外)
        if is_phpunit_test_symbol(qualname, sym.kind, self.lang_id) {
            return true;
        }
        // qualname ベースでも Python unittest 規約をチェック (`Foo.test_bar` 等)
        if self.exclude_framework_entrypoints
            && is_python_test_symbol(
                qualname,
                sym.kind,
                self.lang_id,
                self.file_path,
                sym.container.as_deref(),
                &self.unittest_classes,
            )
        {
            return true;
        }

        false
    }
}

pub(crate) fn filter_exported_symbols(
    syms: &[Symbol],
    root: tree_sitter::Node<'_>,
    source: &[u8],
    lang_id: crate::language::LangId,
    exclude_trait_impls: bool,
    exclude_framework_entrypoints: bool,
    file_path: Option<&str>,
) -> Vec<(String, String, String)> {
    let context = ExportSurfaceContext::new(
        syms,
        root,
        source,
        lang_id,
        exclude_trait_impls,
        exclude_framework_entrypoints,
        file_path,
    );
    let mut result = Vec::new();
    for sym in syms {
        if context.is_excluded_before_qualname(sym) {
            continue;
        }
        let qualname = context.qualname(sym);
        if context.is_excluded_by_qualname(sym, &qualname) {
            continue;
        }
        if context.rust_owner_type_is_crate_internal(sym, &qualname) {
            continue;
        }
        // 除外判定後に署名を作り、不要な文字列構築を避ける。
        let sig = extract_api_signature(sym, root, source, &context.lines, lang_id);
        result.push((qualname, format!("{:?}", sym.kind).to_lowercase(), sig));
    }
    result
}

/// `qualname` (例: `Class.method` や bare name `foo`) が `callees` に含まれるかを判定する。
/// Python/Ruby など「obj.method()」形式で呼び出される言語では callee 側は bare name のみ
/// なので、qualname の末尾 (`.` 区切りの最後) でも判定する。
pub(crate) fn is_internally_connected(
    callees: &std::collections::HashSet<String>,
    qualname: &str,
) -> bool {
    if callees.contains(qualname) {
        return true;
    }
    if let Some(bare) = qualname.rsplit('.').next()
        && bare != qualname
        && callees.contains(bare)
    {
        return true;
    }
    false
}
