//! api.rm の降格 (removed_dead) で、JS/TS の参照 1 件を構文上の位置と字句束縛で分類する。
//!
//! 参照の帰属を (参照ファイル, 名前) 単位の事実で決めると、`api.fetchUser` のメンバー名や
//! `a.helper()` の名前空間経由の呼び出しまで、ファイル内の同名定義や別モジュールからの同名
//! import に帰属させてしまう。ここでは出現ごとに、
//! - import / from 句付き export の中の出現は、その文の解決先と公開名で
//! - メンバー名・分割代入のキーは、受け手の由来が分からないので未証明
//! - オブジェクトのキーや引数の宣言名のように削除シンボルを指し得ない位置は対象外
//! - 式・型の位置の識別子は、字句スコープで束縛を解決して
//!
//! 分類する。証明できない出現は `Unproven` にする (fail-closed)。

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use tree_sitter::Node;

use super::js_ts_shadow::{Namespace, ScopeBinding, resolve_scope_binding};
use super::removed_attribution::relative_import_candidates;
use super::ts_alias_resolution::{AliasResolution, TsAliasResolver};
use crate::language::LangId;

/// 参照 1 件の分類。
#[derive(Clone)]
pub(crate) enum JsRefSite {
    /// 削除シンボルを指し得ない位置 (オブジェクトのキー、クラスのフィールド名、型のプロパティ名、
    /// JSX の属性名、引数などの束縛の宣言名)。
    NonUse,
    /// メンバー名・分割代入のキー。受け手の由来を証明できない限り未証明。
    Member,
    /// 外部パッケージの import の中の出現と、その束縛の利用。削除したローカルのシンボルとは別物。
    External,
    /// 相対 import か、tsconfig / jsconfig の別名で解いた import (from 句付き export を含む) の
    /// 束縛。`candidates` は実際の解決先を含む候補。`export_proven` は、存在する全ての解決候補が
    /// その公開名を由来の分かる形 (同じファイルの通常の宣言、または同名の明示的な再 export) で
    /// export していること。`export *` だけの転送は由来が分からないので含めない。
    Import {
        candidates: Rc<HashSet<String>>,
        export_proven: bool,
    },
    /// 同じファイルの通常の宣言 (関数・クラス・型・独立した初期化子の const)。
    LocalDeclaration,
    /// 判定できない (束縛が無い・由来の分からない束縛・解決できない import など)。
    Unproven,
}

/// import 文が導入する束縛の公開名。
#[derive(Clone, PartialEq, Eq, Hash)]
enum ImportedName {
    Named(String),
    Default,
    Namespace,
}

/// モジュールの公開名の由来を確かめた結果と、相対でない指定子の解決器のキャッシュ (検出 1 回に
/// 閉じる)。
#[derive(Default)]
pub(crate) struct JsExportOriginCache {
    proven: HashMap<(String, ImportedName, String), bool>,
    /// 削除したシンボルのファイル (`dir` 相対)。
    removed_files: HashSet<String>,
    aliases: Option<TsAliasResolver>,
}

impl JsExportOriginCache {
    pub(crate) fn new(removed_files: HashSet<String>) -> Self {
        Self {
            removed_files,
            ..Self::default()
        }
    }

    /// `ref_path` のファイルにある相対でない指定子を、`dir` の tsconfig / jsconfig と
    /// package.json で解く。解決器は最初に使うときに作る。
    fn resolve_alias(&mut self, dir: &str, ref_path: &str, spec: &str) -> AliasResolution {
        self.resolver(dir).resolve(ref_path, spec)
    }

    /// 相対の指定子を [`relative_import_candidates`] で網羅できるか (どの設定も
    /// `moduleSuffixes`・`rootDirs` で相対の探索を変えない)。
    pub(crate) fn relative_resolution_unchanged(&mut self, dir: &str) -> bool {
        self.resolver(dir).relative_resolution_unchanged()
    }

    fn resolver(&mut self, dir: &str) -> &mut TsAliasResolver {
        let removed_files = &self.removed_files;
        self.aliases
            .get_or_insert_with(|| TsAliasResolver::load(dir, removed_files.clone()))
    }
}

/// `symbol` と同じテキストを持つ出現を位置ごとに分類する。相対でない import の解決先と外部
/// パッケージの判定は、`dir` の tsconfig / jsconfig と package.json で行う ([`TsAliasResolver`])。
pub(crate) fn classify_js_ref_sites(
    dir: &str,
    ref_path: &str,
    source: &[u8],
    root: Node<'_>,
    symbol: &str,
    cache: &mut JsExportOriginCache,
) -> HashMap<(usize, usize), JsRefSite> {
    let context = SiteContext {
        dir,
        ref_path,
        source,
        symbol,
    };
    let mut sites = HashMap::new();
    let mut nodes = vec![root];
    while let Some(node) = nodes.pop() {
        if matches!(
            node.kind(),
            "identifier"
                | "property_identifier"
                | "type_identifier"
                | "shorthand_property_identifier"
                | "shorthand_property_identifier_pattern"
                | "statement_identifier"
        ) && node.utf8_text(source).ok() == Some(symbol)
        {
            let position = (node.start_position().row, node.start_position().column);
            sites.insert(position, context.classify(node, cache));
        }
        let mut cursor = node.walk();
        nodes.extend(node.named_children(&mut cursor));
    }
    sites
}

struct SiteContext<'a> {
    dir: &'a str,
    ref_path: &'a str,
    source: &'a [u8],
    symbol: &'a str,
}

fn field_is(parent: Node<'_>, field: &str, node: Node<'_>) -> bool {
    parent
        .child_by_field_name(field)
        .is_some_and(|child| child.id() == node.id())
}

impl SiteContext<'_> {
    fn text<'n>(&self, node: Node<'n>) -> Option<&str> {
        node.utf8_text(self.source).ok()
    }

    fn classify(&self, node: Node<'_>, cache: &mut JsExportOriginCache) -> JsRefSite {
        if let Some(statement) = enclosing_module_statement(node) {
            return self.classify_in_module_statement(statement, node, cache);
        }
        let Some(parent) = node.parent() else {
            return JsRefSite::Unproven;
        };
        match node.kind() {
            "statement_identifier" => JsRefSite::NonUse,
            // 分割代入のキー (`const { helper } = api` / `{ helper = 1 } = api`) は受け手の
            // プロパティを取り出す。束縛の宣言でもあるが、値の由来は受け手次第。
            "shorthand_property_identifier_pattern" => JsRefSite::Member,
            "property_identifier" => match parent.kind() {
                "member_expression" if field_is(parent, "property", node) => JsRefSite::Member,
                "pair_pattern" if field_is(parent, "key", node) => JsRefSite::Member,
                "pair" if field_is(parent, "key", node) => JsRefSite::NonUse,
                "method_definition"
                | "public_field_definition"
                | "field_definition"
                | "property_signature"
                | "method_signature"
                | "abstract_method_signature"
                | "enum_assignment"
                    if field_is(parent, "name", node) =>
                {
                    JsRefSite::NonUse
                }
                "enum_body" | "jsx_attribute" => JsRefSite::NonUse,
                _ => JsRefSite::Unproven,
            },
            "type_identifier"
                if parent.kind() == "nested_type_identifier" && field_is(parent, "name", node) =>
            {
                JsRefSite::Member
            }
            "identifier" if is_binding_declaration(node) => JsRefSite::NonUse,
            "identifier" | "type_identifier" | "shorthand_property_identifier" => {
                self.classify_lexical(node, cache)
            }
            _ => JsRefSite::Unproven,
        }
    }

    /// 式・型の位置の識別子を字句スコープで解決する。
    fn classify_lexical(&self, node: Node<'_>, cache: &mut JsExportOriginCache) -> JsRefSite {
        let namespace = if node.kind() == "type_identifier" {
            Namespace::Type
        } else {
            Namespace::Value
        };
        match resolve_scope_binding(node, self.source, self.symbol, namespace) {
            Some(ScopeBinding::Import(statement)) => {
                match local_import_binding(statement, self.source, self.symbol) {
                    Some(imported) => self.classify_import(statement, imported, cache),
                    None => JsRefSite::Unproven,
                }
            }
            Some(
                ScopeBinding::FunctionDeclaration
                | ScopeBinding::ClassDeclaration
                | ScopeBinding::TypeDeclaration,
            ) => JsRefSite::LocalDeclaration,
            Some(ScopeBinding::Variable(declarator)) if independent_const(declarator) => {
                JsRefSite::LocalDeclaration
            }
            _ => JsRefSite::Unproven,
        }
    }

    /// import 文 / from 句付き export 文の中の出現。
    fn classify_in_module_statement(
        &self,
        statement: Node<'_>,
        node: Node<'_>,
        cache: &mut JsExportOriginCache,
    ) -> JsRefSite {
        let Some(parent) = node.parent() else {
            return JsRefSite::Unproven;
        };
        let imported = match parent.kind() {
            "import_specifier" | "export_specifier" => {
                let Some(name) = parent.child_by_field_name("name") else {
                    return JsRefSite::Unproven;
                };
                if name.id() == node.id() || field_is(parent, "alias", node) {
                    match self.text(name) {
                        Some("default") => ImportedName::Default,
                        Some(name) => ImportedName::Named(name.to_string()),
                        None => return JsRefSite::Unproven,
                    }
                } else {
                    return JsRefSite::Unproven;
                }
            }
            "import_clause" => ImportedName::Default,
            "namespace_import" | "namespace_export" => ImportedName::Namespace,
            _ => return JsRefSite::Unproven,
        };
        self.classify_import(statement, imported, cache)
    }

    /// import の束縛を、文の解決先と公開名で分類する。
    fn classify_import(
        &self,
        statement: Node<'_>,
        imported: ImportedName,
        cache: &mut JsExportOriginCache,
    ) -> JsRefSite {
        let Some(spec) = statement
            .child_by_field_name("source")
            .and_then(|source| super::static_js_string_text(source, self.source))
        else {
            return JsRefSite::Unproven;
        };
        let candidates = match relative_import_candidates(self.ref_path, spec) {
            // 設定 (`moduleSuffixes`・`rootDirs`) やディレクトリの package.json (`types`・
            // `typesVersions`) が探索を変えると、候補で網羅できない。
            Some(_)
                if !cache.relative_resolution_unchanged(self.dir)
                    || super::removed_attribution::relative_directory_has_manifest(
                        self.dir,
                        self.ref_path,
                        spec,
                    ) =>
            {
                return JsRefSite::Unproven;
            }
            Some(candidates) => Rc::new(candidates),
            // 相対でない指定子は、tsconfig / jsconfig の paths・baseUrl を先に解き、ローカルに
            // 解かれなければ package.json の依存を見る。依存の名前でも別名で差し替わり得るので、
            // 名前だけで外部とはしない。
            None => match cache.resolve_alias(self.dir, self.ref_path, spec) {
                AliasResolution::Local(candidates) => candidates,
                AliasResolution::External => return JsRefSite::External,
                AliasResolution::Unknown => return JsRefSite::Unproven,
            },
        };
        let export_proven = export_proven(self.dir, &candidates, &imported, self.symbol, cache);
        JsRefSite::Import {
            candidates,
            export_proven,
        }
    }
}

/// `node` を含む import 文、または from 句付き export 文 (再 export)。
fn enclosing_module_statement(node: Node<'_>) -> Option<Node<'_>> {
    let mut current = node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "import_statement" => return Some(parent),
            "export_statement" => {
                return parent.child_by_field_name("source").map(|_| parent);
            }
            "program" | "statement_block" | "class_body" => return None,
            _ => current = parent.parent(),
        }
    }
    None
}

/// 束縛の宣言名 (引数・catch・ループ変数・分割代入の束縛の位置) か。refs はこれらを
/// `ref` として返すが、別モジュールのシンボルを指すことはない。
fn is_binding_declaration(node: Node<'_>) -> bool {
    let mut current = node;
    while let Some(parent) = current.parent() {
        match parent.kind() {
            "required_parameter" | "optional_parameter" => {
                return field_is(parent, "pattern", current);
            }
            "formal_parameters" => return true,
            "arrow_function" => return field_is(parent, "parameter", current),
            "catch_clause" => return field_is(parent, "parameter", current),
            "for_in_statement" => return field_is(parent, "left", current),
            "variable_declarator" => return field_is(parent, "name", current),
            // 既定値の右辺 (`(x = helper)`) は参照なので、左辺から上るときだけ続ける。
            "assignment_pattern" | "object_assignment_pattern" => {
                if !field_is(parent, "left", current) {
                    return false;
                }
            }
            "array_pattern" | "rest_pattern" | "object_pattern" => {}
            _ => return false,
        }
        current = parent;
    }
    false
}

/// `const name = <独立した初期化子>` か。関数・クラス式・リテラルだけを独立とみなす
/// (呼び出し結果や別の値の代入は、削除モジュールから来た値かもしれない)。
fn independent_const(declarator: Node<'_>) -> bool {
    let Some(declaration) = declarator.parent() else {
        return false;
    };
    if declaration.kind() != "lexical_declaration" {
        return false;
    }
    let mut cursor = declaration.walk();
    if !declaration
        .children(&mut cursor)
        .any(|child| child.kind() == "const")
    {
        return false;
    }
    declarator
        .child_by_field_name("value")
        .is_some_and(independent_value)
}

fn independent_value(mut value: Node<'_>) -> bool {
    // 括弧・`as` などの入れ子は入力次第で深くなるため、再帰せずループで内側へ辿る。
    loop {
        match value.kind() {
            "arrow_function"
            | "function_expression"
            | "function"
            | "generator_function"
            | "class"
            | "number"
            | "string"
            | "true"
            | "false"
            | "null"
            | "undefined"
            | "regex" => return true,
            "template_string" => {
                let mut cursor = value.walk();
                return !value
                    .named_children(&mut cursor)
                    .any(|child| child.kind() == "template_substitution");
            }
            "parenthesized_expression"
            | "as_expression"
            | "satisfies_expression"
            | "non_null_expression" => match value.named_child(0) {
                Some(inner) => value = inner,
                None => return false,
            },
            _ => return false,
        }
    }
}

/// import 文の中で `symbol` をローカルに束縛する指定子の公開名。
fn local_import_binding(statement: Node<'_>, source: &[u8], symbol: &str) -> Option<ImportedName> {
    let mut cursor = statement.walk();
    let clause = statement
        .named_children(&mut cursor)
        .find(|child| child.kind() == "import_clause")?;
    let mut clause_cursor = clause.walk();
    for child in clause.named_children(&mut clause_cursor) {
        match child.kind() {
            "identifier" if child.utf8_text(source).ok() == Some(symbol) => {
                return Some(ImportedName::Default);
            }
            "namespace_import" => {
                let mut ns = child.walk();
                if child
                    .named_children(&mut ns)
                    .any(|n| n.kind() == "identifier" && n.utf8_text(source).ok() == Some(symbol))
                {
                    return Some(ImportedName::Namespace);
                }
            }
            "named_imports" => {
                let mut specifiers = child.walk();
                for specifier in child
                    .named_children(&mut specifiers)
                    .filter(|n| n.kind() == "import_specifier")
                {
                    let name = specifier.child_by_field_name("name")?;
                    let local = specifier.child_by_field_name("alias").unwrap_or(name);
                    if local.utf8_text(source).ok() == Some(symbol) {
                        return match name.utf8_text(source).ok()? {
                            "default" => Some(ImportedName::Default),
                            imported => Some(ImportedName::Named(imported.to_string())),
                        };
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// 存在する全ての解決候補が、`imported` を由来の分かる形で公開しているか。存在する候補が
/// 無ければ false。
fn export_proven(
    dir: &str,
    candidates: &HashSet<String>,
    imported: &ImportedName,
    symbol: &str,
    cache: &mut JsExportOriginCache,
) -> bool {
    let mut existing: Vec<&String> = candidates
        .iter()
        .filter(|candidate| std::path::Path::new(dir).join(candidate).is_file())
        .collect();
    if existing.is_empty() {
        return false;
    }
    existing.sort();
    existing.into_iter().all(|candidate| {
        let key = (candidate.clone(), imported.clone(), symbol.to_string());
        if let Some(proven) = cache.proven.get(&key) {
            return *proven;
        }
        let proven = module_export_proven(dir, candidate, imported, symbol);
        cache.proven.insert(key, proven);
        proven
    })
}

/// モジュール `module_path` が `imported` を、由来の分かる形で export しているか。
fn module_export_proven(
    dir: &str,
    module_path: &str,
    imported: &ImportedName,
    symbol: &str,
) -> bool {
    if *imported == ImportedName::Namespace {
        // 名前空間オブジェクト自体は削除シンボルではない (中身のメンバー参照は別の出現)。
        return true;
    }
    let full = std::path::Path::new(dir).join(module_path);
    let Some(utf8) = camino::Utf8Path::from_path(&full) else {
        return false;
    };
    let Ok(lang) = LangId::from_path(utf8) else {
        return false;
    };
    if !matches!(lang, LangId::Javascript | LangId::Typescript | LangId::Tsx) {
        return false;
    }
    let Ok(source) = crate::engine::parser::read_file(utf8) else {
        return false;
    };
    let Ok(tree) = crate::engine::parser::parse_source(&source, lang) else {
        return false;
    };
    let root = tree.root_node();
    if root.has_error() {
        return false;
    }
    let export_name = match imported {
        ImportedName::Named(name) => name.as_str(),
        ImportedName::Default => "default",
        ImportedName::Namespace => return true,
    };
    let mut found = false;
    let mut cursor = root.walk();
    for statement in root
        .named_children(&mut cursor)
        .filter(|node| node.kind() == "export_statement")
    {
        match export_statement_origin(
            statement,
            &source,
            root,
            export_name,
            symbol,
            super::ts_module_paths::is_declaration_path(module_path),
        ) {
            Some(true) => found = true,
            Some(false) => return false,
            None => {}
        }
    }
    found
}

/// export 文が `export_name` を公開していれば、その由来を確かめられたか (Some(true)) /
/// 確かめられないか (Some(false))。公開していなければ None。
fn export_statement_origin(
    statement: Node<'_>,
    source: &[u8],
    root: Node<'_>,
    export_name: &str,
    symbol: &str,
    declaration_file: bool,
) -> Option<bool> {
    let text = |node: Node<'_>| node.utf8_text(source).ok();
    let mut tokens = statement.walk();
    let is_default = statement
        .children(&mut tokens)
        .any(|child| child.kind() == "default");
    if let Some(declaration) = statement.child_by_field_name("declaration") {
        let declaration = declared_in_file(declaration, declaration_file).unwrap_or(declaration);
        if is_default {
            return (export_name == "default").then(|| ordinary_declaration(declaration));
        }
        return declaration_exports(declaration, source, export_name).then(|| {
            ordinary_declaration(declaration)
                || independent_lexical(declaration)
                || (declaration_file && declares_in_declaration_file(declaration))
        });
    }
    if is_default {
        if export_name != "default" {
            return None;
        }
        let value = statement.child_by_field_name("value")?;
        return Some(if value.kind() == "identifier" {
            text(value).is_some_and(|name| {
                top_level_binding_proven(root, source, name, symbol, declaration_file)
            })
        } else {
            independent_value(value) || value.kind() == "object"
        });
    }
    let has_source = statement.child_by_field_name("source").is_some();
    let mut result = None;
    let mut cursor = statement.walk();
    for clause in statement
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "export_clause")
    {
        let mut specifiers = clause.walk();
        for specifier in clause
            .named_children(&mut specifiers)
            .filter(|child| child.kind() == "export_specifier")
        {
            let Some(name) = specifier.child_by_field_name("name").and_then(text) else {
                return Some(false);
            };
            let alias = specifier.child_by_field_name("alias").and_then(text);
            if alias.unwrap_or(name) != export_name {
                continue;
            }
            let proven = if has_source {
                // 同名の明示的な再 export (`export { helper } from "./src"`) は、その出現が
                // 削除シンボルの参照として別に検査される。名前を変える再 export や、削除シンボルと
                // 別の名前の転送は検査されないので証明しない。
                name == symbol && alias.is_none_or(|alias| alias == name)
            } else {
                top_level_binding_proven(root, source, name, symbol, declaration_file)
            };
            result = Some(result.unwrap_or(true) && proven);
        }
    }
    result
}

/// export 宣言が `export_name` を宣言するか。
fn declaration_exports(declaration: Node<'_>, source: &[u8], export_name: &str) -> bool {
    if declaration
        .child_by_field_name("name")
        .is_some_and(|name| name.utf8_text(source).ok() == Some(export_name))
    {
        return true;
    }
    if matches!(
        declaration.kind(),
        "lexical_declaration" | "variable_declaration"
    ) {
        let mut cursor = declaration.walk();
        return declaration
            .named_children(&mut cursor)
            .filter(|child| child.kind() == "variable_declarator")
            .any(|declarator| {
                declarator
                    .child_by_field_name("name")
                    .is_some_and(|pattern| {
                        super::js_ts_shadow::pattern_binds_name(pattern, source, export_name)
                    })
            });
    }
    false
}

/// 宣言ファイルの `declare` の宣言 (`export declare class Name {}`) の中の宣言。宣言ファイルでは
/// `declare` が普通の書き方で、そのファイルの宣言を表す。namespace と `global` は含めない。
fn declared_in_file(node: Node<'_>, declaration_file: bool) -> Option<Node<'_>> {
    if !declaration_file || node.kind() != "ambient_declaration" {
        return None;
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| ordinary_declaration(*child) || declares_in_declaration_file(*child))
}

/// 宣言ファイルの中の、初期化子の無い宣言 (`declare const name: T`・`declare function name()`)。
fn declares_in_declaration_file(declaration: Node<'_>) -> bool {
    matches!(
        declaration.kind(),
        "lexical_declaration" | "variable_declaration" | "function_signature"
    )
}

/// 実体を同じファイルに持つ通常の宣言 (`declare` だけの宣言や namespace は含めない)。
fn ordinary_declaration(declaration: Node<'_>) -> bool {
    matches!(
        declaration.kind(),
        "function_declaration"
            | "generator_function_declaration"
            | "class_declaration"
            | "abstract_class_declaration"
            | "interface_declaration"
            | "type_alias_declaration"
            | "enum_declaration"
            | "class"
            | "function_expression"
            | "function"
            | "generator_function"
            | "arrow_function"
    )
}

/// `export const name = <独立した初期化子>` (単純な名前の declarator だけ)。
fn independent_lexical(declaration: Node<'_>) -> bool {
    if declaration.kind() != "lexical_declaration" {
        return false;
    }
    let mut cursor = declaration.walk();
    let declarators: Vec<_> = declaration
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "variable_declarator")
        .collect();
    !declarators.is_empty()
        && declarators.iter().all(|declarator| {
            declarator
                .child_by_field_name("name")
                .is_some_and(|name| name.kind() == "identifier")
                && independent_const(*declarator)
        })
}

/// モジュール直下の束縛 `name` が、同じファイルの通常の宣言か独立した初期化子の const か。
/// `name` が削除シンボルと同じ名前で import されている場合は、その import の出現が別に
/// 検査されるので由来の分かる転送とみなす。
fn top_level_binding_proven(
    root: Node<'_>,
    source: &[u8],
    name: &str,
    symbol: &str,
    declaration_file: bool,
) -> bool {
    let mut found = None;
    let mut cursor = root.walk();
    for statement in root.named_children(&mut cursor) {
        let candidate = match statement.kind() {
            "export_statement" => statement.child_by_field_name("declaration"),
            _ => Some(statement),
        };
        let Some(node) = candidate else {
            continue;
        };
        let binding = match node.kind() {
            "function_declaration"
            | "generator_function_declaration"
            | "class_declaration"
            | "abstract_class_declaration"
            | "interface_declaration"
            | "type_alias_declaration"
            | "enum_declaration" => node
                .child_by_field_name("name")
                .is_some_and(|n| n.utf8_text(source).ok() == Some(name))
                .then_some(true),
            "lexical_declaration" | "variable_declaration" => {
                declaration_exports(node, source, name).then(|| independent_lexical(node))
            }
            "import_statement" => local_import_binding(node, source, name).map(|imported| {
                name == symbol && imported == ImportedName::Named(symbol.to_string())
            }),
            "ambient_declaration"
                if declared_in_file(node, declaration_file).is_some_and(|inner| {
                    inner
                        .child_by_field_name("name")
                        .is_some_and(|n| n.utf8_text(source).ok() == Some(name))
                        || declaration_exports(inner, source, name)
                }) =>
            {
                Some(true)
            }
            "ambient_declaration" | "expression_statement" => {
                // `declare function name` や `namespace name {}` は由来を確かめない。
                let mut inner = node.walk();
                node.named_children(&mut inner)
                    .any(|child| {
                        child
                            .child_by_field_name("name")
                            .is_some_and(|n| n.utf8_text(source).ok() == Some(name))
                    })
                    .then_some(false)
            }
            _ => None,
        };
        if let Some(proven) = binding {
            if found.is_some() {
                // 同名の束縛が複数あれば、どちらが公開されるか確かめない。
                return false;
            }
            found = Some(proven);
        }
    }
    found.unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sites(dir: &str, source: &str, lang: LangId, symbol: &str) -> Vec<(usize, usize, String)> {
        let tree = crate::engine::parser::parse_source(source.as_bytes(), lang).unwrap();
        let mut cache = JsExportOriginCache::default();
        let mut out: Vec<_> = classify_js_ref_sites(
            dir,
            "src/main.ts",
            source.as_bytes(),
            tree.root_node(),
            symbol,
            &mut cache,
        )
        .into_iter()
        .map(|((row, column), site)| {
            let label = match site {
                JsRefSite::NonUse => "non_use".to_string(),
                JsRefSite::Member => "member".to_string(),
                JsRefSite::External => "external".to_string(),
                JsRefSite::Import { export_proven, .. } => format!("import:{export_proven}"),
                JsRefSite::LocalDeclaration => "local".to_string(),
                JsRefSite::Unproven => "unproven".to_string(),
            };
            (row, column, label)
        })
        .collect();
        out.sort();
        out
    }

    /// 出現の位置ごとの分類。メンバー名は未証明、外部パッケージの束縛は対象外、
    /// ローカルの通常の宣言・独立した const への束縛だけを同じファイルの宣言とする。
    #[test]
    fn classifies_occurrences_by_position_and_binding() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("package.json"),
            r#"{ "dependencies": { "datefmt": "^1.0.0" } }"#,
        )
        .unwrap();
        std::fs::write(root.join("src/b.ts"), "export function helper() {}\n").unwrap();
        std::fs::write(
            root.join("src/star.ts"),
            "export * from \"./a\";\nexport class Tool { helper() {} }\n",
        )
        .unwrap();
        let dir = root.to_str().unwrap();
        let source = "import * as a from \"./a\";\nimport { helper as h } from \"./b\";\nimport { format } from \"datefmt\";\nexport function run(helper: number) {\n  return [a.helper(), format(1), { helper: 1 }];\n}\nconst { helper } = a;\n";
        let got = sites(dir, source, LangId::Typescript, "helper");
        assert_eq!(
            got,
            vec![
                (1, 9, "import:true".to_string()),
                (3, 20, "non_use".to_string()),
                (4, 12, "member".to_string()),
                (4, 35, "non_use".to_string()),
                (6, 8, "member".to_string()),
            ]
        );
        let got = sites(
            dir,
            "import { format } from \"datefmt\";\nformat(1);\n",
            LangId::Typescript,
            "format",
        );
        assert!(got.iter().all(|(_, _, site)| site == "external"), "{got:?}");
        // 同じファイルの関数宣言・独立した const はローカルの宣言、引数・呼び出し結果は未証明
        let got = sites(
            dir,
            "function helper() {}\nhelper();\nconst k = () => 1;\nk();\n",
            LangId::Typescript,
            "helper",
        );
        assert_eq!(got.last().map(|s| s.2.as_str()), Some("local"));
        let got = sites(
            dir,
            "const helper = make();\nhelper();\n",
            LangId::Typescript,
            "helper",
        );
        assert_eq!(got.last().map(|s| s.2.as_str()), Some("unproven"));
        // `export *` と別の名前のメソッドしか無いモジュールからの import は証明しない
        let got = sites(
            dir,
            "import { helper } from \"./star\";\nhelper();\n",
            LangId::Typescript,
            "helper",
        );
        assert!(
            got.iter().all(|(_, _, site)| site == "import:false"),
            "{got:?}"
        );
    }

    /// 修正前の再帰と同じ形の参照実装 (浅い入力でだけ使う)。
    fn independent_value_recursive(value: Node<'_>) -> bool {
        match value.kind() {
            "arrow_function"
            | "function_expression"
            | "function"
            | "generator_function"
            | "class"
            | "number"
            | "string"
            | "true"
            | "false"
            | "null"
            | "undefined"
            | "regex" => true,
            "template_string" => {
                let mut cursor = value.walk();
                !value
                    .named_children(&mut cursor)
                    .any(|child| child.kind() == "template_substitution")
            }
            "parenthesized_expression"
            | "as_expression"
            | "satisfies_expression"
            | "non_null_expression" => value
                .named_child(0)
                .is_some_and(independent_value_recursive),
            _ => false,
        }
    }

    /// ループ版が、浅い入力の宣言のすべての部分木で再帰版と同じ判定を返す。
    #[test]
    fn independent_value_matches_recursive_version() {
        let source = "const a = ((() => 1) as Fn)!;\nconst b = ((make()) satisfies T);\n\
                      const c = (`x${y}`);\nconst d = ((`plain`));\nconst e = (((class {})));\n\
                      const f = ((null)!);\nconst g = (obj.x as unknown as T);\nconst h = (/re/);\n";
        let tree =
            crate::engine::parser::parse_source(source.as_bytes(), LangId::Typescript).unwrap();
        let mut checked = 0;
        let _: std::ops::ControlFlow<()> = crate::engine::tree_walk::walk_preorder(
            tree.root_node(),
            crate::engine::tree_walk::Children::All,
            |node, _| {
                assert_eq!(
                    independent_value(node),
                    independent_value_recursive(node),
                    "{}",
                    node.to_sexp()
                );
                checked += 1;
                std::ops::ControlFlow::Continue(crate::engine::tree_walk::Walk::Descend)
            },
        );
        assert!(checked > 50);
    }
}
