//! 入力に書かず import で取り込むファイル (Issue #288)。
//!
//! 出力の設定は、入力 (`files`・`include`) のファイルが取り込むファイルの宣言も出す。TypeScript の
//! プログラムと同じく、入力から取り込みを辿って届くと確かめられたファイルを、その設定が宣言を出す
//! ファイルとみなす。入力は TypeScript と同じく列挙する (`allowJs`・同じ名前の拡張子の優先・`outDir`
//! の既定の除外)。辿るのは、先頭の文の import / export / `import x = require()`・モジュールの拡張、
//! 式と型の `import()`、`allowJs` の JS の `require()`、先頭の `/// <reference path>` で、指定子ごとに
//! TypeScript が実際に選ぶ 1 つのファイルを、探索の順に先の候補が無いことを確かめて決める。既定の
//! 解決の方式が TypeScript の版 (5 と 6) で変わる設定は、あり得る方式のどれでも同じファイルに解ける
//! ときだけ採る。宣言ファイルと JS も辿る (宣言を出すのは TS のソースだけで、呼び出し側で限る)。
//!
//! 確かめられないもの (`paths`・`baseUrl` のある設定の相対でない指定子、package.json のある
//! ディレクトリ、symlink、node_modules の外の `typeRoots`、project references、読めないファイル、
//! 上限) があれば、届かないとは言い切らない ([`Reach::Unknown`])。node_modules の下 (外のライブラリ)
//! は宣言を出さないので辿らない。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use serde_json::Value;
use tree_sitter::Node;

use super::super::config_fs::{ConfigFs, Listing};
use super::super::{ConfigLoader, Paths, Substitution, option_path};
use super::{CASE_INSENSITIVE, Inputs, OptionSetting, components, normalize_lexically};

/// 1 つのグラフのファイルの数の上限。
const MAX_GRAPH_FILES: usize = 10_000;
/// 版ごとに読むソースの数・合計の大きさ・1 つの大きさの上限。
const MAX_PARSED_FILES: usize = 20_000;
const MAX_PARSED_BYTES: usize = 128 * 1024 * 1024;
const MAX_SOURCE_BYTES: u64 = 2 * 1024 * 1024;
/// 入力の列挙で見るディレクトリの要素の数の上限 (グラフごと)。
const MAX_WALK_ENTRIES: usize = 50_000;
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
/// TypeScript 6 の既定の `target` (`ES2025`)。比べるのは ES2015 以上かだけ。
const LATEST_STANDARD: u32 = 2025;

/// TypeScript が取り除く拡張子 (`removeFileExtension` の順)。
const KNOWN_EXTENSIONS: [&str; 12] = [
    ".d.ts", ".d.mts", ".d.cts", ".mjs", ".mts", ".cjs", ".cts", ".ts", ".js", ".tsx", ".jsx",
    ".json",
];

/// 取り込みのグラフでファイルに届くか。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Reach {
    Reached,
    NotReached,
    Unknown,
}

/// 設定 1 つのプログラムのファイル (取り込みのグラフ)。
pub(super) struct Graph {
    /// 届くと確かめたファイル ([`key`])。入力を含む。
    reached: HashSet<String>,
    /// 辿れない取り込み・読めないファイル・上限があり、届かないと言い切れない。
    incomplete: bool,
}

impl Graph {
    pub(super) fn unknown() -> Self {
        Self {
            reached: HashSet::new(),
            incomplete: true,
        }
    }

    /// 正規化したパス `file` に届くか。
    pub(super) fn reach(&self, file: &Path) -> Reach {
        if self.reached.contains(&key(file)) {
            Reach::Reached
        } else if self.incomplete {
            Reach::Unknown
        } else {
            Reach::NotReached
        }
    }
}

/// 比べるためのパスの鍵 (大文字小文字を区別しないファイルシステムでは小文字にする)。
fn key(path: &Path) -> String {
    fold(&path.to_string_lossy())
}

fn fold(text: &str) -> String {
    if CASE_INSENSITIVE {
        text.to_lowercase()
    } else {
        text.to_owned()
    }
}

/// 解決の方式 (相対の指定子の探索の違い)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// node10: TS と宣言ファイルの探索 (ファイルとディレクトリ) を JS より先に終える。
    Node10,
    /// bundler: TS と JS を 1 度に探す。
    Bundler,
    /// node16・nodenext: ESM の探索 (参照元の形式と構文で決まる) は拡張子を補わず、ディレクトリを
    /// 解かない。
    Node16,
    /// classic: ファイルだけを探す。
    Classic,
}

/// TypeScript の版 1 つでの解決の方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Variant {
    kind: Kind,
    /// `resolveJsonModule` の実効値。
    resolve_json: bool,
}

/// `module` の値 (解決の方式の既定を決める区分)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Module {
    /// none・amd・umd・system。
    Legacy,
    CommonJs,
    /// es2015・es2020・es2022・esnext。
    Es,
    /// node16・node18。
    Node,
    /// node20・nodenext (`resolveJsonModule` の既定が有効)。
    NodeJson,
    Preserve,
}

fn module_kind(text: &str) -> Option<Module> {
    Some(match text {
        "none" | "amd" | "umd" | "system" => Module::Legacy,
        "commonjs" => Module::CommonJs,
        "es6" | "es2015" | "es2020" | "es2022" | "esnext" => Module::Es,
        "node16" | "node18" => Module::Node,
        "node20" | "nodenext" => Module::NodeJson,
        "preserve" => Module::Preserve,
        _ => return None,
    })
}

fn resolution_kind(text: &str) -> Option<Kind> {
    Some(match text {
        "classic" => Kind::Classic,
        "node" | "node10" => Kind::Node10,
        "node16" | "nodenext" => Kind::Node16,
        "bundler" => Kind::Bundler,
        _ => return None,
    })
}

/// `target` の年 (ES3 は 3、ES5 は 5、ESNext は最大)。
fn target_level(text: &str) -> Option<u32> {
    match text {
        "es3" => Some(3),
        "es5" => Some(5),
        "es6" => Some(2015),
        "esnext" => Some(u32::MAX),
        _ => text
            .strip_prefix("es")
            .and_then(|year| year.parse().ok())
            .filter(|year| (2015..3000).contains(year)),
    }
}

/// TypeScript 5 (`v6` が false) か 6 の、解決の方式と `resolveJsonModule` の既定。`module` が無ければ
/// `target` から (TS 5 は既定の ES5、TS 6 は既定の ES2025)、`moduleResolution` が無ければ `module`
/// から決まる。node16 系の方式で `module` が node 系でも preserve でもなければ None (TypeScript の
/// 誤り)。
fn variant(
    module: Option<Module>,
    resolution: Option<Kind>,
    target: Option<u32>,
    json: Option<bool>,
    v6: bool,
) -> Option<Variant> {
    let module = module.unwrap_or_else(|| {
        let target = match target {
            Some(target) if target != 3 => target,
            _ if v6 => LATEST_STANDARD,
            _ => 5,
        };
        if target >= 2015 {
            Module::Es
        } else {
            Module::CommonJs
        }
    });
    let kind = resolution.unwrap_or(match (module, v6) {
        (Module::Node | Module::NodeJson, _) => Kind::Node16,
        (Module::Legacy, _) => Kind::Classic,
        (Module::CommonJs, false) => Kind::Node10,
        (Module::Preserve, false) => Kind::Bundler,
        (Module::Es, false) => Kind::Classic,
        (_, true) => Kind::Bundler,
    });
    if kind == Kind::Node16 && !matches!(module, Module::Node | Module::NodeJson | Module::Preserve)
    {
        return None;
    }
    Some(Variant {
        kind,
        resolve_json: json.unwrap_or(module == Module::NodeJson || kind == Kind::Bundler),
    })
}

/// グラフを作るのに使う、設定の実効の値。
pub(super) struct Options {
    /// あり得る解決の方式 (TypeScript の版で変わる既定)。
    variants: Vec<Variant>,
    allow_js: bool,
    jsx: bool,
    no_resolve: bool,
    allow_arbitrary_extensions: bool,
    /// 実効の `paths` (相対でない指定子を先に解く)。
    paths: Option<Paths>,
    /// 実効の `baseUrl` (`paths` に当たらない相対でない指定子を、その下で先に解く)。
    base_url: Option<PathBuf>,
    /// `importHelpers` で足す取り込み (tslib)。
    helpers: Option<String>,
    /// JSX の runtime の取り込み (ソースの `@jsxRuntime`・`@jsxImportSource` の pragma で変わる)。
    jsx_runtime: Option<String>,
    /// `isolatedModules` か `verbatimModuleSyntax` (宣言ファイルでないソースに合成の取り込みを足す)。
    isolated_modules: bool,
    /// `moduleDetection` (小文字)。
    module_detection: Option<String>,
    /// node_modules の外の `typeRoots` (自動の型の取り込みと型の参照がリポジトリのファイルを足し
    /// 得る)。
    local_type_roots: Vec<PathBuf>,
    /// 相対の `types`・相対の JSX の runtime (どのファイルを足すかを確かめない)。
    unsupported_types: bool,
}

impl Options {
    /// 設定 `config` (正規化したパス) の実効の値。相対の探索を変える設定 (既定でない
    /// `moduleSuffixes`・2 つ以上の `rootDirs`)・取り込みを参照先の出力に移す `references`・読めない
    /// 設定・解釈できない値は None。
    pub(super) fn read(loader: &mut ConfigLoader, config: &Path) -> Option<Self> {
        let leaf = config.parent()?.to_path_buf();
        let raw = loader.read(config)?;
        match raw.get("references") {
            None | Some(Value::Null) => {}
            Some(Value::Array(references)) if references.is_empty() => {}
            Some(_) => return None,
        }
        let mut value = |key: &str| -> Option<Option<(Value, PathBuf)>> {
            match loader.option_setting(config, key, 0, &mut Vec::new())? {
                OptionSetting::Value(value, directory) => Some(Some((value, directory))),
                OptionSetting::Unset | OptionSetting::Reset => Some(None),
            }
        };
        if let Some((suffixes, _)) = value("moduleSuffixes")? {
            let suffixes = suffixes.as_array()?;
            if !(suffixes.is_empty() || (suffixes.len() == 1 && suffixes[0].as_str() == Some(""))) {
                return None;
            }
        }
        if let Some((roots, _)) = value("rootDirs")?
            && roots.as_array()?.len() > 1
        {
            return None;
        }
        let flag = |setting: Option<(Value, PathBuf)>| -> Option<Option<bool>> {
            match setting {
                None => Some(None),
                Some((Value::Bool(value), _)) => Some(Some(value)),
                Some(_) => None,
            }
        };
        let text = |setting: Option<(Value, PathBuf)>| -> Option<Option<String>> {
            match setting {
                None => Some(None),
                Some((Value::String(text), _)) => Some(Some(text)),
                Some(_) => None,
            }
        };
        let check_js = flag(value("checkJs")?)?.unwrap_or(false);
        let allow_js = flag(value("allowJs")?)?.unwrap_or(check_js);
        let no_resolve = flag(value("noResolve")?)?.unwrap_or(false);
        let allow_arbitrary_extensions = flag(value("allowArbitraryExtensions")?)?.unwrap_or(false);
        let import_helpers = flag(value("importHelpers")?)?.unwrap_or(false);
        let isolated_modules = flag(value("isolatedModules")?)?.unwrap_or(false)
            || flag(value("verbatimModuleSyntax")?)?.unwrap_or(false);
        let module_detection =
            text(value("moduleDetection")?)?.map(|detection| detection.to_ascii_lowercase());
        let resolve_json = flag(value("resolveJsonModule")?)?;
        let jsx = text(value("jsx")?)?.map(|jsx| jsx.to_ascii_lowercase());
        let jsx_import_source = text(value("jsxImportSource")?)?;
        let module = match text(value("module")?)? {
            Some(module) => Some(module_kind(&module.to_ascii_lowercase())?),
            None => None,
        };
        let resolution = match text(value("moduleResolution")?)? {
            Some(resolution) => Some(resolution_kind(&resolution.to_ascii_lowercase())?),
            None => None,
        };
        let target = match text(value("target")?)? {
            Some(target) => Some(target_level(&target.to_ascii_lowercase())?),
            None => None,
        };
        let base_url = match value("baseUrl")? {
            None => None,
            Some((Value::String(base), directory)) => Some(option_path(&base, &directory, &leaf)?),
            Some(_) => return None,
        };
        let paths = match value("paths")? {
            None => None,
            Some((Value::Object(map), directory)) => {
                let mut entries = Vec::new();
                for (key, values) in &map {
                    let values = values
                        .as_array()?
                        .iter()
                        .map(|value| value.as_str().map(str::to_owned))
                        .collect::<Option<Vec<_>>>()?;
                    // TypeScript が誤りとして報告する形 (`*` が 2 個以上、空の置換先) は解かない。
                    if key.matches('*').count() > 1
                        || values.is_empty()
                        || values
                            .iter()
                            .any(|value| value.matches('*').count() > 1 || value.contains('\\'))
                    {
                        return None;
                    }
                    entries.push((key.clone(), values));
                }
                // 置換先の基点は実効の `baseUrl`、無ければ `paths` を書いた設定のディレクトリ。
                Some(Paths {
                    entries,
                    base: base_url.clone().unwrap_or(directory),
                    leaf: leaf.clone(),
                })
            }
            Some(_) => return None,
        };
        let mut local_type_roots = Vec::new();
        if let Some((roots, directory)) = value("typeRoots")? {
            for root in roots.as_array()? {
                let root = normalize_lexically(&option_path(root.as_str()?, &directory, &leaf)?)?;
                if !has_node_modules(&root) {
                    local_type_roots.push(root);
                }
            }
        }
        let mut unsupported_types = match value("types")? {
            None => false,
            Some((Value::Array(types), _)) => types.iter().any(|name| {
                name.as_str()
                    .is_none_or(|name| name.starts_with('.') || is_rooted(name))
            }),
            Some(_) => return None,
        };
        let helpers = import_helpers.then(|| "tslib".to_owned());
        let mut jsx_runtime = None;
        if matches!(jsx.as_deref(), Some("react-jsx" | "react-jsxdev"))
            || jsx_import_source.is_some()
        {
            let base = jsx_import_source.unwrap_or_else(|| "react".to_owned());
            let runtime = if jsx.as_deref() == Some("react-jsxdev") {
                "jsx-dev-runtime"
            } else {
                "jsx-runtime"
            };
            // 相対の runtime はソースごとに解かれる (どのソースに足すかまでは確かめない)。
            if base.starts_with('.') || is_rooted(&base) {
                unsupported_types = true;
            } else {
                jsx_runtime = Some(format!("{base}/{runtime}"));
            }
        }
        let mut variants = Vec::new();
        for v6 in [false, true] {
            let variant = variant(module, resolution, target, resolve_json, v6)?;
            if !variants.contains(&variant) {
                variants.push(variant);
            }
        }
        Some(Self {
            variants,
            allow_js,
            jsx: jsx.is_some(),
            no_resolve,
            allow_arbitrary_extensions,
            paths,
            base_url,
            helpers,
            jsx_runtime,
            isolated_modules,
            module_detection,
            local_type_roots,
            unsupported_types,
        })
    }
}

/// ソース 1 つの取り込み (TypeScript が解決する指定子)。
#[derive(Debug, PartialEq, Eq)]
struct Imports {
    edges: Vec<Edge>,
    /// 先頭の文に import・export がある (か `import.meta` を使う)。TypeScript の合成の取り込みを
    /// 足すかが変わる。
    module: bool,
    /// JSX の runtime を選ぶ pragma (`@jsxRuntime`・`@jsxImportSource`) がある。
    jsx_pragma: bool,
    /// 宣言ファイルで宣言した ambient module の名前 (同じファイルの取り込みは、それに解かれ
    /// ファイルを探さない)。
    ambient_modules: Vec<String>,
    /// 扱えない形 (解析の誤り・解けない文字列の指定子・JS の JSDoc の import・JSX の pragma など) が
    /// 無い。
    complete: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Edge {
    /// 指定子 (`\` は `/` にする)。
    spec: String,
    kind: EdgeKind,
}

/// 取り込みの構文 (node16 系の方式で、ESM と CJS のどちらで解くかが変わる)。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum EdgeKind {
    /// import・export の宣言・モジュールの拡張・型の `import()` (参照元の形式で解く)。
    Static,
    /// `import x = require()`・JS の `require()` (CJS で解く)。
    Require,
    /// 値の `import()` (ESM で解く)。
    Dynamic,
    /// ESM でも CJS でも同じに解けるときだけ採る (`resolution-mode` の指定・式か型か決められない
    /// `import()`)。
    Either,
    /// `/// <reference path>` (パスそのもの)。
    Reference,
    /// `/// <reference types>`。
    Types,
}

/// 2 つの版のソースの取り込みが違うか (Issue #288。違えば、差分に無いソースを取り込む設定が変わり
/// 得る)。解析できなければ true。
pub(crate) fn imports_differ(path: &str, old: &[u8], new: &[u8]) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    let extract = |source: &[u8]| {
        std::str::from_utf8(source)
            .ok()
            .and_then(|source| extract_imports(name, source))
    };
    match (extract(old), extract(new)) {
        (Some(old), Some(new)) => old != new,
        _ => true,
    }
}

/// ファイル名 `name` (拡張子で言語を決める) のソースの取り込み。TS・JS のソースでないか、解析でき
/// なければ None。
fn extract_imports(name: &str, source: &str) -> Option<Imports> {
    let extension = known_extension(name)?;
    if extension == ".json" {
        return None;
    }
    let javascript = is_js_extension(extension);
    let lang = if extension == ".tsx" {
        crate::language::LangId::Tsx
    } else if javascript {
        crate::language::LangId::Javascript
    } else {
        crate::language::LangId::Typescript
    };
    let tree = crate::engine::parser::parse_source(source.as_bytes(), lang).ok()?;
    let root = tree.root_node();
    let mut collector = Collector {
        bytes: source.as_bytes(),
        edges: Vec::new(),
        complete: !root.has_error(),
        declaration: crate::commands::api_changes::ts_module_paths::is_declaration_path(name),
        module: has_module_syntax(root) || has_import_meta(root, source.as_bytes()),
        ambient_modules: Vec::new(),
    };
    collector.statements(root, false);
    collector.expressions(root, javascript);
    collector.pragmas(source);
    // JSX の runtime を選ぶ pragma は、ソースごとに取り込みを変える。
    let jsx_pragma = source.contains("@jsxImportSource") || source.contains("@jsxRuntime");
    if jsx_pragma {
        collector.complete = false;
    }
    let mut ambient_modules = collector.ambient_modules;
    ambient_modules.sort();
    ambient_modules.dedup();
    // 同じファイルで宣言した ambient module の名前の取り込みは、TypeScript がファイルを探さない
    // (`resolveToOwnAmbientModule`)。
    let mut edges: Vec<Edge> = collector
        .edges
        .into_iter()
        .filter(|edge| {
            matches!(edge.kind, EdgeKind::Reference | EdgeKind::Types)
                || ambient_modules.binary_search(&edge.spec).is_err()
        })
        .collect();
    edges.sort();
    edges.dedup();
    Some(Imports {
        edges,
        module: collector.module,
        jsx_pragma,
        ambient_modules,
        complete: collector.complete,
    })
}

/// `import.meta` を使うか (TypeScript は `moduleDetection` によらずモジュールの根拠にする)。
fn has_import_meta(root: Node<'_>, bytes: &[u8]) -> bool {
    if !bytes
        .windows(b"import".len())
        .any(|window| window == b"import")
    {
        return false;
    }
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "meta_property"
            && node
                .utf8_text(bytes)
                .is_ok_and(|text| text.starts_with("import"))
        {
            return true;
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    false
}

/// 先頭の文に import・export があるか (TypeScript のモジュールのファイル)。
fn has_module_syntax(root: Node<'_>) -> bool {
    let mut cursor = root.walk();
    root.named_children(&mut cursor)
        .any(|statement| matches!(statement.kind(), "import_statement" | "export_statement"))
}

/// 型の位置 (`import("…")` は参照元の形式で解く)。
const TYPE_CONTEXTS: &[&str] = &[
    "type_annotation",
    "type_alias_declaration",
    "type_query",
    "type_arguments",
    "type_parameters",
    "type_parameter",
    "constraint",
    "default_type",
    "implements_clause",
    "extends_type_clause",
    "opting_type_annotation",
    "omitting_type_annotation",
    "adding_type_annotation",
    "asserts_annotation",
    "type_predicate_annotation",
    "index_signature",
    "property_signature",
    "method_signature",
    "call_signature",
    "construct_signature",
    "abstract_method_signature",
    "function_signature",
    "interface_body",
    "object_type",
    "union_type",
    "intersection_type",
    "function_type",
    "constructor_type",
    "parenthesized_type",
    "array_type",
    "tuple_type",
    "optional_type",
    "rest_type",
    "generic_type",
    "conditional_type",
    "lookup_type",
    "index_type_query",
    "readonly_type",
    "literal_type",
    "template_literal_type",
    "mapped_type_clause",
    "infer_type",
    "type_predicate",
    "asserts",
];

/// 文字列の指定子。
enum Literal {
    Text(String),
    /// 文字列でない (TypeScript は解かない)。
    Computed,
    /// 解けないエスケープ。
    Unsupported,
}

struct Collector<'a> {
    bytes: &'a [u8],
    edges: Vec<Edge>,
    complete: bool,
    declaration: bool,
    /// 先頭の文に import・export がある (か `import.meta` を使う)。
    module: bool,
    /// 宣言ファイルの、スクリプトの先頭で宣言した ambient module の名前。
    ambient_modules: Vec<String>,
}

impl Collector<'_> {
    fn push(&mut self, spec: String, kind: EdgeKind) {
        if !spec.is_empty() {
            self.edges.push(Edge { spec, kind });
        }
    }

    fn text(&self, node: Node<'_>) -> Option<&str> {
        node.utf8_text(self.bytes).ok()
    }

    /// 文字列・置換の無いテンプレートの値 (`\` は `/` にする)。
    fn literal(&self, node: Node<'_>) -> Literal {
        let Some(raw) = self.text(node) else {
            return Literal::Unsupported;
        };
        let template = match node.kind() {
            "string" => false,
            "template_string" => true,
            _ => return Literal::Computed,
        };
        if template
            && node
                .named_children(&mut node.walk())
                .any(|child| child.kind() == "template_substitution")
        {
            return Literal::Computed;
        }
        let Some(inner) = raw.get(1..raw.len().saturating_sub(1)) else {
            return Literal::Unsupported;
        };
        let mut text = String::new();
        let mut chars = inner.chars();
        while let Some(character) = chars.next() {
            if character != '\\' {
                text.push(character);
                continue;
            }
            match chars.next() {
                Some(escaped @ ('\\' | '\'' | '"' | '/' | '`')) => text.push(escaped),
                _ => return Literal::Unsupported,
            }
        }
        Literal::Text(text.replace('\\', "/"))
    }

    /// 指定子の文字列 `node`。解けないエスケープは扱えない形にする。
    fn module_name(&mut self, node: Node<'_>, kind: EdgeKind, ambient: bool) {
        match self.literal(node) {
            Literal::Text(spec) => {
                // ambient module の中の相対の指定子は、TypeScript が集めない。
                if !(ambient && is_relative(&spec)) {
                    self.push(spec, kind);
                }
            }
            Literal::Computed => {}
            Literal::Unsupported => self.complete = false,
        }
    }

    /// 文の並び `parent` (ファイルか ambient module の本体) の取り込み。
    fn statements(&mut self, parent: Node<'_>, ambient: bool) {
        let mut cursor = parent.walk();
        let statements: Vec<Node<'_>> = parent.named_children(&mut cursor).collect();
        for (index, statement) in statements.iter().enumerate() {
            self.statement(*statement, statements.get(index + 1).copied(), ambient);
        }
    }

    fn statement(&mut self, node: Node<'_>, next: Option<Node<'_>>, ambient: bool) {
        match node.kind() {
            "import_statement" => {
                let require = node
                    .named_children(&mut node.walk())
                    .find(|child| child.kind() == "import_require_clause");
                match require {
                    Some(clause) => {
                        if let Some(source) = clause.child_by_field_name("source") {
                            self.module_name(source, EdgeKind::Require, ambient);
                        }
                    }
                    None => {
                        if let Some(source) = node.child_by_field_name("source") {
                            self.module_name(source, static_kind(node, self.bytes), ambient);
                        }
                    }
                }
            }
            "export_statement" => {
                if let Some(source) = node.child_by_field_name("source") {
                    self.module_name(source, static_kind(node, self.bytes), ambient);
                } else if let Some(alias) = node
                    .named_children(&mut node.walk())
                    .find(|child| child.kind() == "import_alias")
                {
                    self.export_import_require(alias, next, ambient);
                }
            }
            "ambient_declaration" => {
                let modules: Vec<Node<'_>> = node
                    .named_children(&mut node.walk())
                    .filter(|child| child.kind() == "module")
                    .collect();
                for module in modules {
                    self.module_declaration(module, ambient);
                }
            }
            // 宣言ファイルと ambient module の中では、`declare` の無い `module "x" {}` も ambient。
            "module" if self.declaration || ambient => self.module_declaration(node, ambient),
            _ => {}
        }
    }

    /// `export import x = require("…")`。文法は `import_alias` と次の式の文に分けて読む。間に `;`
    /// のある `export import x = require; ("…");` (`require` という名前空間の別名と式の文) とは
    /// 区別し、間の空白とコメントは読み飛ばす。
    fn export_import_require(&mut self, alias: Node<'_>, next: Option<Node<'_>>, ambient: bool) {
        let Some(require) = alias
            .named_children(&mut alias.walk())
            .filter(|child| child.kind() != "comment")
            .last()
            .filter(|last| last.kind() == "identifier" && self.text(*last) == Some("require"))
        else {
            return;
        };
        let mut next = next;
        while let Some(comment) = next.filter(|next| next.kind() == "comment") {
            next = comment.next_named_sibling();
        }
        let Some(next) = next.filter(|next| next.kind() == "expression_statement") else {
            return;
        };
        match self
            .bytes
            .get(require.end_byte()..next.start_byte())
            .and_then(|between| std::str::from_utf8(between).ok())
            .map(is_trivia)
        {
            Some(true) => {}
            Some(false) => return,
            None => {
                self.complete = false;
                return;
            }
        }
        let source = next
            .named_child(0)
            .filter(|expression| {
                expression.kind() == "parenthesized_expression"
                    && expression.named_child_count() == 1
            })
            .and_then(|expression| expression.named_child(0))
            .filter(|source| source.kind() == "string");
        match source {
            Some(source) => self.module_name(source, EdgeKind::Require, ambient),
            None => self.complete = false,
        }
    }

    /// `declare module "x" { … }`。TypeScript は、モジュールのファイルの宣言を拡張として名前を解く
    /// だけでファイルを足さず (取り込みの一覧の外)、そうでなければ ambient module の宣言として本体の
    /// 相対でない取り込みを集める (ambient module の中の宣言は集めない)。宣言ファイルの ambient
    /// module の名前は、本体の無い `declare module "x";` も集める。
    fn module_declaration(&mut self, node: Node<'_>, ambient: bool) {
        if self.module || ambient {
            return;
        }
        let Some(name) = node
            .child_by_field_name("name")
            .filter(|name| name.kind() == "string")
        else {
            return;
        };
        let body = node.child_by_field_name("body");
        if self.declaration {
            if let Literal::Text(name) = self.literal(name) {
                self.ambient_modules.push(name);
            }
            if let Some(body) = body {
                self.statements(body, true);
            }
            return;
        }
        let Some(body) = body else {
            return;
        };
        // 宣言ファイルでなく import・export も無いソースは、`moduleDetection` などでモジュールに
        // なり得る (そのときは拡張で、本体を集めない)。本体に取り込みがあれば決めない。
        let mut inner = Collector {
            bytes: self.bytes,
            edges: Vec::new(),
            complete: true,
            declaration: false,
            module: false,
            ambient_modules: Vec::new(),
        };
        inner.statements(body, true);
        if !inner.edges.is_empty() || !inner.complete {
            self.complete = false;
        }
    }

    /// 式と型の `import()`、JS の `require()`、JS の JSDoc の import。
    fn expressions(&mut self, root: Node<'_>, javascript: bool) {
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            match node.kind() {
                "call_expression" => self.call(node, javascript),
                "comment"
                    if javascript
                        && self.text(node).is_some_and(|comment| {
                            comment.contains("@import") || comment.contains("import(")
                        }) =>
                {
                    self.complete = false;
                }
                _ => {}
            }
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
        }
    }

    fn call(&mut self, node: Node<'_>, javascript: bool) {
        let Some(function) = node.child_by_field_name("function") else {
            return;
        };
        let arguments: Vec<Node<'_>> = node
            .child_by_field_name("arguments")
            .map(|arguments| {
                arguments
                    .named_children(&mut arguments.walk())
                    .filter(|argument| argument.kind() != "comment")
                    .collect()
            })
            .unwrap_or_default();
        let Some(first) = arguments.first().copied() else {
            return;
        };
        if function.kind() == "import" {
            let kind = if javascript {
                EdgeKind::Dynamic
            } else {
                match import_position(node) {
                    Some(true) => {
                        let mode = arguments.get(1).is_some_and(|options| {
                            self.text(*options)
                                .is_some_and(|text| text.contains("resolution-mode"))
                        });
                        if mode {
                            EdgeKind::Either
                        } else {
                            EdgeKind::Static
                        }
                    }
                    Some(false) => EdgeKind::Dynamic,
                    None => EdgeKind::Either,
                }
            };
            self.module_name(first, kind, false);
        } else if javascript
            && arguments.len() == 1
            && function.kind() == "identifier"
            && self.text(function) == Some("require")
        {
            self.module_name(first, EdgeKind::Require, false);
        }
    }

    /// 先頭のコメントの `/// <reference path>` と `/// <reference types>` (TypeScript と同じく
    /// `no-default-lib`・`types`・`lib`・`path` の順に 1 つを採る)。
    fn pragmas(&mut self, source: &str) {
        for comment in leading_comments(source) {
            let Some(rest) = comment.strip_prefix("///") else {
                continue;
            };
            let rest = rest.trim_start_matches(is_js_space);
            let Some(rest) = rest.strip_prefix('<') else {
                continue;
            };
            let Some(name_end) = rest.find(is_js_space) else {
                continue;
            };
            let (name, after) = rest.split_at(name_end);
            let space = after.chars().next().map_or(0, char::len_utf8);
            if !name.eq_ignore_ascii_case("reference") || !after[space..].contains("/>") {
                continue;
            }
            if named_argument(comment, "no-default-lib").as_deref() == Some("true") {
                continue;
            }
            match (
                named_argument(comment, "types"),
                named_argument(comment, "lib"),
                named_argument(comment, "path"),
            ) {
                (Some(types), _, _) => self.push(types.replace('\\', "/"), EdgeKind::Types),
                (None, None, Some(path)) => {
                    self.push(path.replace('\\', "/"), EdgeKind::Reference);
                }
                _ => {}
            }
        }
    }
}

/// import・export の宣言の構文。型だけの宣言の `resolution-mode` は解く方式を変える (決めずに両方で
/// 解く)。
fn static_kind(node: Node<'_>, bytes: &[u8]) -> EdgeKind {
    let mode = node
        .named_children(&mut node.walk())
        .filter(|child| child.kind() == "import_attribute")
        .any(|attribute| {
            attribute
                .utf8_text(bytes)
                .is_ok_and(|text| text.contains("resolution-mode"))
        });
    if mode {
        EdgeKind::Either
    } else {
        EdgeKind::Static
    }
}

/// `import()` が型の位置か (Some(true))、値か (Some(false))。`as`・`satisfies` の中は決めない。
fn import_position(node: Node<'_>) -> Option<bool> {
    let mut current = node.parent();
    while let Some(parent) = current {
        let kind = parent.kind();
        if TYPE_CONTEXTS.contains(&kind) {
            return Some(true);
        }
        if matches!(kind, "as_expression" | "satisfies_expression") {
            return None;
        }
        current = parent.parent();
    }
    Some(false)
}

/// JavaScript の正規表現の `\s`。
fn is_js_space(character: char) -> bool {
    character.is_whitespace() || character == '\u{feff}'
}

/// 空白とコメントだけか (TypeScript の trivia)。
fn is_trivia(text: &str) -> bool {
    let mut rest = text;
    loop {
        rest = rest.trim_start_matches(is_js_space);
        if rest.is_empty() {
            return true;
        }
        if let Some(body) = rest.strip_prefix("//") {
            rest = body
                .find(['\n', '\r', '\u{2028}', '\u{2029}'])
                .map_or("", |end| &body[end..]);
        } else if let Some(body) = rest.strip_prefix("/*") {
            match body.find("*/") {
                Some(end) => rest = &body[end + 2..],
                None => return false,
            }
        } else {
            return false;
        }
    }
}

/// ファイルの先頭のコメント (TypeScript の `getLeadingCommentRanges(text, 0)`)。1 行のコメントは
/// `//` から行末まで。
fn leading_comments(source: &str) -> Vec<&str> {
    let mut rest = source.strip_prefix('\u{feff}').unwrap_or(source);
    if rest.starts_with("#!") {
        rest = rest
            .find(['\n', '\r', '\u{2028}', '\u{2029}'])
            .map_or("", |end| &rest[end..]);
    }
    let mut comments = Vec::new();
    loop {
        rest = rest.trim_start_matches(is_js_space);
        if let Some(body) = rest.strip_prefix("//") {
            let end = body
                .find(['\n', '\r', '\u{2028}', '\u{2029}'])
                .unwrap_or(body.len());
            comments.push(&rest[..end + 2]);
            rest = &body[end..];
        } else if let Some(body) = rest.strip_prefix("/*") {
            match body.find("*/") {
                Some(end) => rest = &body[end + 2..],
                None => break,
            }
        } else {
            break;
        }
    }
    comments
}

/// pragma の引数 `name="…"` (TypeScript の `(\s{name}\s*=\s*)('([^']*)'|"([^"]*)")`。名前は
/// 大文字小文字を区別しない)。
fn named_argument(text: &str, name: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let name: Vec<char> = name.chars().collect();
    for start in 0..chars.len() {
        if !is_js_space(chars[start]) {
            continue;
        }
        let mut index = start + 1;
        if chars.len() < index + name.len()
            || !name
                .iter()
                .zip(&chars[index..])
                .all(|(expected, actual)| expected.eq_ignore_ascii_case(actual))
        {
            continue;
        }
        index += name.len();
        while index < chars.len() && is_js_space(chars[index]) {
            index += 1;
        }
        if chars.get(index) != Some(&'=') {
            continue;
        }
        index += 1;
        while index < chars.len() && is_js_space(chars[index]) {
            index += 1;
        }
        let Some(&quote) = chars.get(index).filter(|quote| matches!(quote, '\'' | '"')) else {
            continue;
        };
        let value_start = index + 1;
        if let Some(length) = chars[value_start..]
            .iter()
            .position(|character| *character == quote)
        {
            return Some(chars[value_start..value_start + length].iter().collect());
        }
    }
    None
}

/// 名前の既知の拡張子 (`removeFileExtension` と同じく大文字小文字を区別する)。
fn known_extension(name: &str) -> Option<&'static str> {
    KNOWN_EXTENSIONS
        .into_iter()
        .find(|extension| ends_with(name, extension))
}

fn ends_with(text: &str, extension: &str) -> bool {
    text.len() > extension.len() && text.ends_with(extension)
}

/// TypeScript の `changeExtension` (既知の拡張子を `extension` に替える)。
fn change_extension(text: &str, extension: &str) -> String {
    match known_extension(text) {
        Some(known) => format!("{}{extension}", &text[..text.len() - known.len()]),
        None => text.to_owned(),
    }
}

fn is_js_extension(extension: &str) -> bool {
    matches!(extension, ".js" | ".jsx" | ".mjs" | ".cjs")
}

fn file_name(path: &Path) -> Option<&str> {
    path.file_name()?.to_str()
}

fn is_declaration_file(path: &Path) -> bool {
    file_name(path).is_some_and(crate::commands::api_changes::ts_module_paths::is_declaration_path)
}

/// TypeScript の相対の指定子 (`.`・`..`・`./…`・`../…`)。`\` は `/` にしてある。
fn is_relative(spec: &str) -> bool {
    spec == "." || spec == ".." || spec.starts_with("./") || spec.starts_with("../")
}

/// 根からのパス・ドライブ・URL の指定子 (TypeScript は相対と同じくパスとして解く)。
fn is_rooted(spec: &str) -> bool {
    let bytes = spec.as_bytes();
    spec.starts_with('/')
        || spec.starts_with('\\')
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
        || spec.contains("://")
}

/// パスの途中に node_modules がある (外のライブラリ)。
fn has_node_modules(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str() == "node_modules")
}

/// 探索で試す拡張子の種類 (TypeScript の `Extensions`)。
#[derive(Clone, Copy)]
struct Flags {
    typescript: bool,
    declaration: bool,
    javascript: bool,
    json: bool,
}

/// 拡張子 `original` (補うときは空) の指定子に、TypeScript が順に試す拡張子
/// (`tryAddingExtensions`)。`declaration_name` は、元の拡張子まで含めて宣言ファイルの名前か。
fn suffixes(original: &str, flags: Flags, declaration_name: bool) -> Vec<String> {
    let mut list: Vec<&str> = Vec::new();
    let mut add = |enabled: bool, extensions: &[&'static str]| {
        if enabled {
            list.extend_from_slice(extensions);
        }
    };
    match original {
        ".mjs" | ".mts" | ".d.mts" => {
            add(flags.typescript, &[".mts"]);
            add(flags.declaration, &[".d.mts"]);
            add(flags.javascript, &[".mjs"]);
        }
        ".cjs" | ".cts" | ".d.cts" => {
            add(flags.typescript, &[".cts"]);
            add(flags.declaration, &[".d.cts"]);
            add(flags.javascript, &[".cjs"]);
        }
        ".json" => {
            add(flags.declaration, &[".d.json.ts"]);
            add(flags.json, &[".json"]);
        }
        ".tsx" | ".jsx" => {
            add(flags.typescript, &[".tsx", ".ts"]);
            add(flags.declaration, &[".d.ts"]);
            add(flags.javascript, &[".jsx", ".js"]);
        }
        ".ts" | ".d.ts" | ".js" | "" => {
            add(flags.typescript, &[".ts", ".tsx"]);
            add(flags.declaration, &[".d.ts"]);
            add(flags.javascript, &[".js", ".jsx"]);
        }
        other => {
            return if flags.declaration && !declaration_name {
                vec![format!(".d{other}.ts")]
            } else {
                Vec::new()
            };
        }
    }
    list.into_iter().map(str::to_owned).collect()
}

/// 解決の結果。
#[derive(Clone, Debug, PartialEq, Eq)]
enum Resolution {
    /// 解いたファイル (正規化したパス) と、探索で付けた拡張子。
    Found(PathBuf, String),
    NotFound,
    Unknown,
}

/// 取り込みでプログラムに足すもの。
#[derive(Clone, Debug, PartialEq, Eq)]
enum Outcome {
    /// 足すファイル (正規化したパス)。
    Added(PathBuf),
    /// 足さない (解けない・外のライブラリ・`allowJs` の無い JS・JSON など)。
    Nothing,
    Unknown,
}

/// 版ごとに共有する読み取り (ソースの取り込み・存在・package.json)。
#[derive(Default)]
pub(super) struct ReachCache {
    imports: HashMap<PathBuf, Option<Rc<Imports>>>,
    files: HashMap<PathBuf, Option<bool>>,
    directories: HashMap<PathBuf, Option<bool>>,
    /// ディレクトリ → 子の名前 ([`fold`] したもの。読めなければ None)。
    listings: HashMap<PathBuf, Option<HashSet<String>>>,
    canonical: HashMap<PathBuf, Option<PathBuf>>,
    /// ディレクトリ → 途中に symlink・junction が無いか ([`Self::unlinked`])。
    unlinked: HashMap<PathBuf, Option<bool>>,
    /// package.json → 内容 (無ければ Some(None)、読めなければ None)。
    manifests: HashMap<PathBuf, Option<Option<Rc<Value>>>>,
    parsed_files: usize,
    parsed_bytes: usize,
}

impl ReachCache {
    /// ディレクトリ `directory` までの途中に symlink・junction が無いか (正規化しても同じパスか)。
    /// TypeScript は相対の指定子の先を実体のパスにしないので、リンクを通るファイルからは、実体の
    /// 位置とは違うファイルを取り込む。無いディレクトリは Some(true) (その下の候補も無い)。確かめ
    /// られなければ None。
    fn unlinked(&mut self, fs: &ConfigFs, directory: &Path) -> Option<bool> {
        if let Some(&cached) = self.unlinked.get(directory) {
            return cached;
        }
        let result = match fs.is_directory(directory) {
            Some(true) => fs
                .canonicalize(directory)
                .map(|canonical| key(&canonical) == key(directory)),
            Some(false) => Some(true),
            None => None,
        };
        self.unlinked.insert(directory.to_path_buf(), result);
        result
    }

    /// `path` の親のディレクトリまでにリンクが無いことを確かめられるか。
    fn parent_unlinked(&mut self, fs: &ConfigFs, path: &Path) -> bool {
        match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => {
                self.unlinked(fs, parent) == Some(true)
            }
            _ => true,
        }
    }
    /// 親のディレクトリの一覧に名前があるか。一覧を読めないか、ASCII でない名前 (Unicode の正規化を
    /// 区別しないファイルシステムがある) は None。
    fn listed(&mut self, fs: &ConfigFs, path: &Path) -> Option<bool> {
        let parent = path.parent()?;
        let name = file_name(path)?;
        if !name.is_ascii() {
            return None;
        }
        let names = self
            .listings
            .entry(parent.to_path_buf())
            .or_insert_with(|| match fs.read_dir(parent) {
                Listing::Entries(names) => Some(names.iter().map(|name| fold(name)).collect()),
                Listing::Missing => Some(HashSet::new()),
                Listing::Unreadable => None,
            });
        names.as_ref().map(|names| names.contains(&fold(name)))
    }

    /// symlink でない通常のファイルがあるか (途中のリンクを通るか、確かめられなければ None)。
    fn is_file(&mut self, fs: &ConfigFs, path: &Path) -> Option<bool> {
        if let Some(&cached) = self.files.get(path) {
            return cached;
        }
        let result = if !self.parent_unlinked(fs, path) {
            None
        } else {
            match self.listed(fs, path) {
                Some(false) => Some(false),
                _ => fs.is_regular_file(path),
            }
        };
        self.files.insert(path.to_path_buf(), result);
        result
    }

    /// symlink でないディレクトリがあるか (途中のリンクを通るか、確かめられなければ None)。
    fn is_directory(&mut self, fs: &ConfigFs, path: &Path) -> Option<bool> {
        if let Some(&cached) = self.directories.get(path) {
            return cached;
        }
        let result = if !self.parent_unlinked(fs, path) {
            None
        } else {
            match self.listed(fs, path) {
                Some(false) => Some(false),
                _ => fs.is_directory(path),
            }
        };
        self.directories.insert(path.to_path_buf(), result);
        result
    }

    fn canonicalize(&mut self, fs: &ConfigFs, path: &Path) -> Option<PathBuf> {
        self.canonical
            .entry(path.to_path_buf())
            .or_insert_with(|| fs.canonicalize(path))
            .clone()
    }

    /// ソース `path` (正規化したパス) の取り込み。読めない・解析できない・上限を超えれば None。
    fn imports(&mut self, fs: &ConfigFs, path: &Path) -> Option<Rc<Imports>> {
        if let Some(cached) = self.imports.get(path) {
            return cached.clone();
        }
        let imports = self.read_imports(fs, path);
        self.imports.insert(path.to_path_buf(), imports.clone());
        imports
    }

    fn read_imports(&mut self, fs: &ConfigFs, path: &Path) -> Option<Rc<Imports>> {
        self.parsed_files += 1;
        if self.parsed_files > MAX_PARSED_FILES {
            return None;
        }
        let source = fs.read_utf8_limited(path, MAX_SOURCE_BYTES)?;
        self.parsed_bytes += source.len();
        if self.parsed_bytes > MAX_PARSED_BYTES {
            return None;
        }
        extract_imports(file_name(path)?, &source).map(Rc::new)
    }

    /// package.json `path` の内容。無ければ Some(None)、読めなければ None。
    fn manifest(&mut self, fs: &ConfigFs, path: &Path) -> Option<Option<Rc<Value>>> {
        if let Some(cached) = self.manifests.get(path) {
            return cached.clone();
        }
        let manifest = match self.is_file(fs, path) {
            None => None,
            Some(false) => Some(None),
            Some(true) => fs
                .read_utf8_limited(path, MAX_MANIFEST_BYTES)
                .and_then(|text| {
                    let bytes = crate::commands::api_changes::jsonc::json_bytes(
                        text.trim_start_matches('\u{feff}'),
                    )?;
                    serde_json::from_slice::<Value>(&bytes).ok()
                })
                .map(|value| Some(Rc::new(value))),
        };
        self.manifests.insert(path.to_path_buf(), manifest.clone());
        manifest
    }

    /// ディレクトリ `directory` から祖先へ、いちばん近い package.json (無ければ Some(None))。
    fn package_scope(&mut self, fs: &ConfigFs, directory: &Path) -> Option<Option<Rc<Value>>> {
        for ancestor in directory.ancestors() {
            if let Some(manifest) = self.manifest(fs, &ancestor.join("package.json"))? {
                return Some(Some(manifest));
            }
        }
        Some(None)
    }

    /// node16 系の方式で、ファイル `file` の形式が ESM か (拡張子か、いちばん近い package.json の
    /// `type`)。
    fn esm_format(&mut self, fs: &ConfigFs, file: &Path) -> Option<bool> {
        let name = file_name(file)?;
        if [".d.mts", ".mts", ".mjs"]
            .iter()
            .any(|extension| name.ends_with(extension))
        {
            return Some(true);
        }
        if [".d.cts", ".cts", ".cjs"]
            .iter()
            .any(|extension| name.ends_with(extension))
        {
            return Some(false);
        }
        let scope = self.package_scope(fs, file.parent()?)?;
        Some(
            scope.is_some_and(|manifest| {
                manifest.get("type").and_then(Value::as_str) == Some("module")
            }),
        )
    }
}

/// 設定のプログラムのファイルを、入力 `inputs` から取り込みを辿って集める。
pub(super) fn build(
    fs: &ConfigFs,
    inputs: &Inputs,
    options: &Options,
    cache: &mut ReachCache,
) -> Graph {
    let mut builder = Builder {
        fs,
        cache,
        options,
        resolutions: HashMap::new(),
        incomplete: options.unsupported_types,
    };
    for root in &options.local_type_roots {
        if builder.cache.is_directory(fs, root) != Some(false) {
            builder.incomplete = true;
        }
    }
    let mut queue = builder.entries(inputs);
    let mut reached = HashSet::new();
    while let Some(file) = queue.pop() {
        if !reached.insert(key(&file)) {
            continue;
        }
        if reached.len() > MAX_GRAPH_FILES {
            builder.incomplete = true;
            break;
        }
        if options.no_resolve {
            continue;
        }
        let Some(imports) = builder.cache.imports(fs, &file) else {
            builder.incomplete = true;
            continue;
        };
        if !imports.complete {
            builder.incomplete = true;
        }
        for edge in &imports.edges {
            match builder.edge(&file, edge) {
                Outcome::Added(target) => queue.push(target),
                Outcome::Nothing => {}
                Outcome::Unknown => builder.incomplete = true,
            }
        }
        // 合成の取り込み (tslib・JSX の runtime) は、TypeScript が JS のファイルと、宣言ファイルで
        // ないモジュール (か `isolatedModules`) に足す。モジュールかを決められない (`moduleDetection`
        // が形式や JSX でモジュールにし得る) ファイルでは、足し得る先があれば決めない。
        let javascript = file_name(&file)
            .and_then(known_extension)
            .is_some_and(is_js_extension);
        let applies = if javascript {
            Some(true)
        } else if is_declaration_file(&file) {
            Some(false)
        } else if options.isolated_modules
            || imports.module
            || options.module_detection.as_deref() == Some("force")
        {
            Some(true)
        } else if options.module_detection.as_deref() == Some("legacy") {
            Some(false)
        } else {
            None
        };
        // JSX の runtime は、ソースの pragma で変わる (`@jsxRuntime classic` は足さない)。
        let synthetic = [
            (options.helpers.as_ref(), applies),
            (
                options.jsx_runtime.as_ref(),
                if imports.jsx_pragma && applies != Some(false) {
                    None
                } else {
                    applies
                },
            ),
        ];
        for (spec, applies) in synthetic {
            let Some(spec) = spec.filter(|_| applies != Some(false)) else {
                continue;
            };
            let edge = Edge {
                spec: spec.clone(),
                kind: EdgeKind::Static,
            };
            match (builder.edge(&file, &edge), applies) {
                (Outcome::Added(target), Some(true)) => queue.push(target),
                (Outcome::Nothing, _) => {}
                _ => builder.incomplete = true,
            }
        }
    }
    Graph {
        reached,
        incomplete: builder.incomplete,
    }
}

struct Builder<'a> {
    fs: &'a ConfigFs,
    cache: &'a mut ReachCache,
    options: &'a Options,
    /// (参照元のディレクトリ, 指定子, 構文, 参照元の拡張子と宣言ファイルか) → 結果。
    resolutions: HashMap<(String, String, EdgeKind, String), Outcome>,
    incomplete: bool,
}

impl Builder<'_> {
    /// 入力のファイル: `files` に書いたもの (`/// <reference path>` と同じ規則) と、`include` に
    /// 当たるもの (同じ名前は拡張子の優先の高いもの)。
    fn entries(&mut self, inputs: &Inputs) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut literal = HashSet::new();
        for file in &inputs.files {
            let path: PathBuf = file.iter().collect();
            literal.insert(key(&path));
            match self.reference_target(&path) {
                Outcome::Added(target) => found.push(target),
                Outcome::Nothing => {}
                Outcome::Unknown => self.incomplete = true,
            }
        }
        if !inputs.include.is_empty() {
            self.walk(inputs, &literal, &mut found);
        }
        found
    }

    /// `include` に当たるファイルを、TypeScript と同じくディレクトリごとに名前の順に見て選ぶ。
    fn walk(&mut self, inputs: &Inputs, literal: &HashSet<String>, found: &mut Vec<PathBuf>) {
        let mut roots: Vec<PathBuf> = Vec::new();
        for pattern in &inputs.include {
            // 先頭の区切り (Windows の正規化したパスの `\\?\C:` など) はワイルドカードではない。
            let wildcard = pattern
                .iter()
                .skip(1)
                .position(|part| part.contains(['*', '?']))
                .map(|index| index + 1);
            let fixed = &pattern[..wildcard.unwrap_or(pattern.len().saturating_sub(1))];
            let root: PathBuf = fixed.iter().collect();
            if !roots.iter().any(|existing| root.starts_with(existing)) {
                roots.retain(|existing| !existing.starts_with(&root));
                roots.push(root);
            }
        }
        let mut stack = Vec::new();
        for root in roots {
            match self.cache.is_directory(self.fs, &root) {
                Some(true) => stack.push(root),
                Some(false) => {}
                None => self.incomplete = true,
            }
        }
        let mut seen = 0usize;
        while let Some(directory) = stack.pop() {
            let mut names = match self.fs.read_dir(&directory) {
                Listing::Entries(names) => names,
                Listing::Missing => continue,
                Listing::Unreadable => {
                    self.incomplete = true;
                    continue;
                }
            };
            names.sort();
            let mut files = Vec::new();
            for name in names {
                seen += 1;
                if seen > MAX_WALK_ENTRIES {
                    self.incomplete = true;
                    return;
                }
                let path = directory.join(&name);
                let parts = components(&path);
                let matched =
                    self.wildcard_extension(&name).is_some() && inputs.wildcard_includes(&parts);
                let descends = inputs.may_descend(&parts) && !inputs.excludes(&parts);
                match self.cache.is_directory(self.fs, &path) {
                    Some(true) => {
                        if descends {
                            stack.push(path);
                        }
                        continue;
                    }
                    Some(false) => {}
                    None => {
                        if matched || descends {
                            self.incomplete = true;
                        }
                        continue;
                    }
                }
                if !matched {
                    continue;
                }
                match self.cache.is_file(self.fs, &path) {
                    Some(true) => files.push(path),
                    Some(false) => {}
                    None => self.incomplete = true,
                }
            }
            for path in self.select(files, literal) {
                match self.cache.canonicalize(self.fs, &path) {
                    Some(canonical) => found.push(canonical),
                    None => self.incomplete = true,
                }
            }
        }
    }

    /// 入力として列挙する拡張子 (大文字小文字を区別する)。
    fn wildcard_extension(&self, name: &str) -> Option<&'static str> {
        let typescript = [".d.ts", ".d.cts", ".d.mts", ".ts", ".tsx", ".cts", ".mts"];
        let javascript = [".js", ".jsx", ".cjs", ".mjs"];
        typescript
            .into_iter()
            .chain(javascript.into_iter().filter(|_| self.options.allow_js))
            .find(|extension| ends_with(name, extension))
    }

    /// 拡張子の組 (TypeScript の `getSupportedExtensions` の組)。
    fn groups(&self) -> [&'static [&'static str]; 3] {
        if self.options.allow_js {
            [
                &[".ts", ".tsx", ".d.ts", ".js", ".jsx"],
                &[".cts", ".d.cts", ".cjs"],
                &[".mts", ".d.mts", ".mjs"],
            ]
        } else {
            [
                &[".ts", ".tsx", ".d.ts"],
                &[".cts", ".d.cts"],
                &[".mts", ".d.mts"],
            ]
        }
    }

    /// 1 つのディレクトリの、`include` に当たるファイル (名前の順) から入力を選ぶ。TypeScript の
    /// `hasFileWithHigherPriorityExtension`・`removeWildcardFilesWithLowerPriorityExtension` と同じく、
    /// 同じ名前は拡張子の優先の高いものを残す (`.d.ts` と `.js` は並べて残す)。
    fn select(&self, files: Vec<PathBuf>, literal: &HashSet<String>) -> Vec<PathBuf> {
        let groups = self.groups();
        let mut chosen: HashMap<String, PathBuf> = HashMap::new();
        for path in files {
            let Some(text) = path.to_str() else {
                continue;
            };
            let Some(group) = groups
                .iter()
                .find(|group| group.iter().any(|extension| ends_with(text, extension)))
            else {
                continue;
            };
            let has = |candidate: &str, chosen: &HashMap<String, PathBuf>| {
                let candidate = fold(candidate);
                literal.contains(&candidate) || chosen.contains_key(&candidate)
            };
            let mut higher = false;
            for extension in group.iter() {
                if ends_with(text, extension) && (*extension != ".ts" || !ends_with(text, ".d.ts"))
                {
                    break;
                }
                if has(&change_extension(text, extension), &chosen) {
                    if *extension == ".d.ts" && (ends_with(text, ".js") || ends_with(text, ".jsx"))
                    {
                        continue;
                    }
                    higher = true;
                    break;
                }
            }
            if higher {
                continue;
            }
            for extension in group.iter().rev() {
                if ends_with(text, extension) {
                    break;
                }
                chosen.remove(&fold(&change_extension(text, extension)));
            }
            let own = fold(text);
            if !literal.contains(&own) && !chosen.contains_key(&own) {
                chosen.insert(own, path.clone());
            }
        }
        chosen.into_values().collect()
    }

    /// 取り込み 1 つ。
    fn edge(&mut self, importer: &Path, edge: &Edge) -> Outcome {
        let Some(directory) = importer.parent() else {
            return Outcome::Unknown;
        };
        let class = format!(
            "{}{}",
            file_name(importer)
                .and_then(known_extension)
                .unwrap_or_default(),
            if is_declaration_file(importer) {
                "!"
            } else {
                ""
            }
        );
        let cache_key = (key(directory), edge.spec.clone(), edge.kind, class);
        if let Some(outcome) = self.resolutions.get(&cache_key) {
            return outcome.clone();
        }
        let outcome = match edge.kind {
            EdgeKind::Reference => {
                if is_rooted(&edge.spec) {
                    Outcome::Unknown
                } else {
                    match normalize_lexically(&directory.join(&edge.spec)) {
                        Some(path) => self.reference_target(&path),
                        None => Outcome::Unknown,
                    }
                }
            }
            // 型の参照は node_modules の `@types` (外のライブラリ) か `typeRoots` に解かれる。
            // node_modules の外の `typeRoots` があれば、グラフを決めない ([`build`])。
            EdgeKind::Types => {
                if is_relative(&edge.spec) || is_rooted(&edge.spec) {
                    Outcome::Unknown
                } else {
                    Outcome::Nothing
                }
            }
            _ if is_rooted(&edge.spec) => Outcome::Unknown,
            _ if is_relative(&edge.spec) => self.relative(importer, directory, edge),
            _ => self.non_relative(importer, directory, edge),
        };
        self.resolutions.insert(cache_key, outcome.clone());
        outcome
    }

    /// `files` のファイルと `/// <reference path>` の先 (`path` は正規化したパス)。拡張子があれば
    /// 対応する拡張子のそのファイル、無ければ `.ts`・`.tsx`・`.d.ts` (`allowJs` なら `.js`・`.jsx`
    /// も) を補う。
    fn reference_target(&mut self, path: &Path) -> Outcome {
        if has_node_modules(path) {
            return Outcome::Nothing;
        }
        let (Some(name), Some(text)) = (file_name(path), path.to_str()) else {
            return Outcome::Unknown;
        };
        if name.contains('.') {
            let name = fold(name);
            let mut supported = vec![".ts", ".tsx", ".d.ts", ".cts", ".d.cts", ".mts", ".d.mts"];
            if self.options.allow_js {
                supported.extend([".js", ".jsx", ".cjs", ".mjs"]);
            }
            if !supported
                .iter()
                .any(|extension| ends_with(&name, extension))
            {
                return Outcome::Nothing;
            }
            return match self.cache.is_file(self.fs, path) {
                Some(true) => match self.cache.canonicalize(self.fs, path) {
                    Some(canonical) => Outcome::Added(canonical),
                    None => Outcome::Unknown,
                },
                Some(false) => Outcome::Nothing,
                None => Outcome::Unknown,
            };
        }
        let mut extensions = vec![".ts", ".tsx", ".d.ts"];
        if self.options.allow_js {
            extensions.extend([".js", ".jsx"]);
        }
        let suffixes: Vec<String> = extensions.into_iter().map(str::to_owned).collect();
        match self.probe(text, &suffixes) {
            Resolution::Found(target, _) => Outcome::Added(target),
            Resolution::NotFound => Outcome::Nothing,
            Resolution::Unknown => Outcome::Unknown,
        }
    }

    /// 候補を順に見て、最初にある通常のファイル。ある・無いを確かめられない候補があれば Unknown。
    fn probe(&mut self, stem: &str, suffixes: &[String]) -> Resolution {
        for suffix in suffixes {
            let candidate = PathBuf::from(format!("{stem}{suffix}"));
            match self.cache.is_file(self.fs, &candidate) {
                Some(true) => {
                    return match self.cache.canonicalize(self.fs, &candidate) {
                        Some(canonical) => Resolution::Found(canonical, suffix.clone()),
                        None => Resolution::Unknown,
                    };
                }
                Some(false) => {}
                None => return Resolution::Unknown,
            }
        }
        Resolution::NotFound
    }

    /// node16 系の方式で、参照元 `importer` の取り込み `kind` を ESM (true) と CJS のどちらで解くか
    /// (ほかの方式は区別しない)。参照元の形式を決められなければ None。
    fn modes(
        &mut self,
        variant: Variant,
        kind: EdgeKind,
        importer: &Path,
    ) -> Option<&'static [bool]> {
        if variant.kind != Kind::Node16 {
            return Some(&[false]);
        }
        Some(match kind {
            EdgeKind::Require => &[false],
            EdgeKind::Dynamic => &[true],
            EdgeKind::Either => &[true, false],
            _ => {
                if self.cache.esm_format(self.fs, importer)? {
                    &[true]
                } else {
                    &[false]
                }
            }
        })
    }

    /// あり得る方式 (と node16 系の ESM・CJS) のどれでも同じ結果のときだけ採る。
    fn agree(
        &mut self,
        importer: &Path,
        kind: EdgeKind,
        mut resolve: impl FnMut(&mut Self, Variant, bool) -> Outcome,
    ) -> Outcome {
        let options = self.options;
        let mut outcome: Option<Outcome> = None;
        for &variant in &options.variants {
            let Some(modes) = self.modes(variant, kind, importer) else {
                return Outcome::Unknown;
            };
            for &esm in modes {
                let result = resolve(self, variant, esm);
                match &outcome {
                    None => outcome = Some(result),
                    Some(previous) if *previous == result => {}
                    Some(_) => return Outcome::Unknown,
                }
            }
        }
        outcome.unwrap_or(Outcome::Unknown)
    }

    /// 相対の指定子。
    fn relative(&mut self, importer: &Path, directory: &Path, edge: &Edge) -> Outcome {
        let Some(path) = normalize_lexically(&directory.join(&edge.spec)) else {
            return Outcome::Unknown;
        };
        // node_modules を通る指定子は外のライブラリ (宣言を出さない)。
        if has_node_modules(&path) {
            return Outcome::Nothing;
        }
        let last = edge.spec.rsplit('/').next().unwrap_or(&edge.spec);
        let directory_only = edge.spec.ends_with('/') || last == "." || last == "..";
        let Some(text) = path.to_str().map(str::to_owned) else {
            return Outcome::Unknown;
        };
        self.agree(importer, edge.kind, |builder, variant, esm| {
            let resolved = builder.resolve(&text, directory_only, variant, esm);
            builder.admit(resolved, importer)
        })
    }

    /// パス `text` (正規化した、指定子の指すパス) を方式 `variant` で解く。node10 と classic は TS
    /// と宣言ファイルを先に、bundler と node16 系は全てを 1 度に探す。
    fn resolve(
        &mut self,
        text: &str,
        directory_only: bool,
        variant: Variant,
        esm: bool,
    ) -> Resolution {
        if variant.kind == Kind::Classic && directory_only {
            return Resolution::Unknown;
        }
        let phases = match variant.kind {
            Kind::Node10 | Kind::Classic => vec![
                Flags {
                    typescript: true,
                    declaration: true,
                    javascript: false,
                    json: false,
                },
                Flags {
                    typescript: false,
                    declaration: false,
                    javascript: true,
                    json: variant.resolve_json,
                },
            ],
            Kind::Bundler | Kind::Node16 => vec![Flags {
                typescript: true,
                declaration: true,
                javascript: true,
                json: variant.resolve_json,
            }],
        };
        for flags in phases {
            match self.resolve_phase(text, directory_only, variant.kind, esm, flags) {
                Resolution::NotFound => {}
                resolved => return resolved,
            }
        }
        Resolution::NotFound
    }

    /// TypeScript の `nodeLoadModuleByRelativeName` (classic は `loadModuleFromFile`)。
    fn resolve_phase(
        &mut self,
        text: &str,
        directory_only: bool,
        kind: Kind,
        esm: bool,
        flags: Flags,
    ) -> Resolution {
        if !directory_only {
            // 拡張子を置き換える (`./a.js` → `a.ts`)。
            if Path::new(text)
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains('.'))
            {
                let (stem, original) = match known_extension(text) {
                    Some(extension) => {
                        (&text[..text.len() - extension.len()], extension.to_owned())
                    }
                    None => {
                        let Some(dot) = text.rfind('.') else {
                            return Resolution::Unknown;
                        };
                        (&text[..dot], text[dot..].to_owned())
                    }
                };
                let declaration_name =
                    crate::commands::api_changes::ts_module_paths::is_declaration_path(
                        &format!("{stem}{original}").replace('\\', "/"),
                    );
                match self.probe(stem, &suffixes(&original, flags, declaration_name)) {
                    Resolution::NotFound => {}
                    resolved => return resolved,
                }
            }
            // 拡張子を補う (`./a` → `a.ts`)。ESM の探索では補わない。
            if !esm {
                match self.probe(text, &suffixes("", flags, false)) {
                    Resolution::NotFound => {}
                    resolved => return resolved,
                }
            }
        }
        if kind == Kind::Classic || esm {
            return Resolution::NotFound;
        }
        let directory = Path::new(text);
        match self.cache.is_directory(self.fs, directory) {
            Some(true) => {}
            Some(false) => return Resolution::NotFound,
            None => return Resolution::Unknown,
        }
        // package.json の `types`・`typings`・`main`・`typesVersions` は扱わない。
        if self.cache.is_file(self.fs, &directory.join("package.json")) != Some(false) {
            return Resolution::Unknown;
        }
        let Some(index) = directory.join("index").to_str().map(str::to_owned) else {
            return Resolution::Unknown;
        };
        self.probe(&index, &suffixes("", flags, false))
    }

    /// 解いたファイルをプログラムに足すか (TypeScript の `getResolutionDiagnostic` と `allowJs`)。
    fn admit(&self, resolved: Resolution, importer: &Path) -> Outcome {
        match resolved {
            Resolution::Unknown => Outcome::Unknown,
            Resolution::NotFound => Outcome::Nothing,
            Resolution::Found(path, extension) => {
                if has_node_modules(&path) {
                    return Outcome::Nothing;
                }
                let added = match extension.as_str() {
                    ".ts" | ".d.ts" | ".mts" | ".d.mts" | ".cts" | ".d.cts" => true,
                    ".tsx" => self.options.jsx,
                    ".jsx" => self.options.jsx && self.options.allow_js,
                    ".js" | ".mjs" | ".cjs" => self.options.allow_js,
                    ".json" => false,
                    _ => self.options.allow_arbitrary_extensions || is_declaration_file(importer),
                };
                if added {
                    Outcome::Added(path)
                } else {
                    Outcome::Nothing
                }
            }
        }
    }

    /// 相対でない指定子。TypeScript と同じく、`paths` (当たったパターンの置換先を順に) か、当たら
    /// なければ `baseUrl` の下を先に探し、無ければ node_modules (外のライブラリ)・package.json の
    /// `imports` と自身の名前 (bundler・node16 系)・classic の祖先のディレクトリへ進む。node10 と
    /// classic は TS と宣言ファイルの段を先に終える。
    fn non_relative(&mut self, importer: &Path, directory: &Path, edge: &Edge) -> Outcome {
        let spec = edge.spec.clone();
        self.agree(importer, edge.kind, |builder, variant, esm| {
            builder.non_relative_in(importer, directory, &spec, variant, esm)
        })
    }

    fn non_relative_in(
        &mut self,
        importer: &Path,
        directory: &Path,
        spec: &str,
        variant: Variant,
        esm: bool,
    ) -> Outcome {
        let typescript = Flags {
            typescript: true,
            declaration: true,
            javascript: false,
            json: false,
        };
        let javascript = Flags {
            typescript: false,
            declaration: false,
            javascript: true,
            json: variant.resolve_json,
        };
        match variant.kind {
            Kind::Bundler | Kind::Node16 => {
                let all = Flags {
                    typescript: true,
                    declaration: true,
                    javascript: true,
                    json: variant.resolve_json,
                };
                match self.mapped(spec, variant.kind, esm, all) {
                    Resolution::NotFound => self.package_local(directory, spec),
                    resolved => self.admit(resolved, importer),
                }
            }
            Kind::Node10 | Kind::Classic => {
                match self.mapped(spec, variant.kind, false, typescript) {
                    Resolution::NotFound => {}
                    resolved => return self.admit(resolved, importer),
                }
                if variant.kind == Kind::Classic
                    && self.classic_ancestors(directory, spec, typescript) != Resolution::NotFound
                {
                    return Outcome::Unknown;
                }
                // JS の段の前に node_modules の型が見つかれば、そこで止まる (確かめない)。JS の段で
                // 足すファイルが見つかるなら決めない。
                let resolved = match self.mapped(spec, variant.kind, false, javascript) {
                    Resolution::NotFound if variant.kind == Kind::Classic => {
                        self.classic_ancestors(directory, spec, javascript)
                    }
                    resolved => resolved,
                };
                match self.admit(resolved, importer) {
                    Outcome::Added(_) => Outcome::Unknown,
                    outcome => outcome,
                }
            }
        }
    }

    /// `paths` と `baseUrl` での解決。どちらも当たらないか、当たった `paths` の置換先がどれも無け
    /// れば NotFound (node_modules などへ進む。`paths` に当たれば `baseUrl` は見ない)。
    fn mapped(&mut self, spec: &str, kind: Kind, esm: bool, flags: Flags) -> Resolution {
        let options = self.options;
        if let Some(paths) = &options.paths {
            match paths.substitutions(spec) {
                None => return Resolution::Unknown,
                Some(Some(substitutions)) => {
                    return self.substitute(&substitutions, kind, esm, flags);
                }
                Some(None) => {}
            }
        }
        let Some(base) = &options.base_url else {
            return Resolution::NotFound;
        };
        let Some(candidate) = normalize_lexically(&base.join(spec)) else {
            return Resolution::Unknown;
        };
        let Some(text) = candidate.to_str().map(str::to_owned) else {
            return Resolution::Unknown;
        };
        self.load(&text, spec.ends_with('/'), kind, esm, flags)
    }

    /// `paths` の置換先を順に解く。拡張子を書いた置換先 (`*` を置き換える前の値で見る) は、まずその
    /// ファイルを見る。
    fn substitute(
        &mut self,
        substitutions: &[Substitution],
        kind: Kind,
        esm: bool,
        flags: Flags,
    ) -> Resolution {
        for substitution in substitutions {
            let Some(candidate) = normalize_lexically(&substitution.path) else {
                return Resolution::Unknown;
            };
            let Some(text) = candidate.to_str().map(str::to_owned) else {
                return Resolution::Unknown;
            };
            if let Some(extension) = substitution.extension {
                match self.cache.is_file(self.fs, &candidate) {
                    Some(true) => {
                        return match self.cache.canonicalize(self.fs, &candidate) {
                            Some(canonical) => Resolution::Found(canonical, extension.to_owned()),
                            None => Resolution::Unknown,
                        };
                    }
                    Some(false) => {}
                    None => return Resolution::Unknown,
                }
            }
            match self.load(&text, substitution.directory_only, kind, esm, flags) {
                Resolution::NotFound => {}
                resolved => return resolved,
            }
        }
        Resolution::NotFound
    }

    /// 置換先・`baseUrl` の下のパスを、TypeScript の loader (`nodeLoadModuleByRelativeName`、classic は
    /// `loadModuleFromFile`) で 1 段だけ解く。
    fn load(
        &mut self,
        text: &str,
        directory_only: bool,
        kind: Kind,
        esm: bool,
        flags: Flags,
    ) -> Resolution {
        if kind == Kind::Classic && directory_only {
            return Resolution::Unknown;
        }
        self.resolve_phase(text, directory_only, kind, esm, flags)
    }

    /// bundler・node16 系で、package.json の `imports` (`#…`) と自身の名前 (`exports` のある
    /// package.json の `name`) の指定子。
    fn package_local(&mut self, directory: &Path, spec: &str) -> Outcome {
        let Some(scope) = self.cache.package_scope(self.fs, directory) else {
            return Outcome::Unknown;
        };
        let Some(manifest) = scope else {
            return Outcome::Nothing;
        };
        if spec.starts_with('#') {
            return if manifest.get("imports").is_some() {
                Outcome::Unknown
            } else {
                Outcome::Nothing
            };
        }
        if manifest.get("exports").is_some()
            && let Some(name) = manifest.get("name").and_then(Value::as_str)
            && (spec == name || spec.starts_with(&format!("{name}/")))
        {
            return Outcome::Unknown;
        }
        Outcome::Nothing
    }

    /// classic の相対でない指定子の 1 段 (`flags`): 参照元から祖先へ、各ディレクトリの下の指定子を
    /// ファイルとして探す。見つかれば (リポジトリのファイルに解かれ得るので) NotFound 以外を返す。
    fn classic_ancestors(&mut self, directory: &Path, spec: &str, flags: Flags) -> Resolution {
        for ancestor in directory.ancestors() {
            let Some(candidate) = normalize_lexically(&ancestor.join(spec)) else {
                return Resolution::Unknown;
            };
            let Some(text) = candidate.to_str().map(str::to_owned) else {
                return Resolution::Unknown;
            };
            match self.resolve_phase(&text, false, Kind::Classic, false, flags) {
                Resolution::NotFound => {}
                resolved => return resolved,
            }
        }
        Resolution::NotFound
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edges(name: &str, source: &str) -> Vec<(String, EdgeKind)> {
        extract_imports(name, source)
            .expect("imports")
            .edges
            .into_iter()
            .map(|edge| (edge.spec, edge.kind))
            .collect()
    }

    #[test]
    fn collects_typescript_imports() {
        let source = "/// <reference path=\"./ref.ts\" />\n/// <reference types=\"node\" />\nimport { a } from \"./a\";\nimport \"./side\";\nimport type { T } from \"./t\" with { \"resolution-mode\": \"import\" };\nimport x = require(\"./x\");\nexport * from \"./star\";\nexport type { U } from \"./u\";\ntype V = import(\"./v\").V;\nconst lazy = import(\"./lazy\");\nconst dynamic = import(name);\ndeclare module \"./augmented\" {}\nfunction f() { require(\"./not-collected\"); }\n";
        assert_eq!(
            edges("a.ts", source),
            vec![
                ("./a".to_owned(), EdgeKind::Static),
                ("./lazy".to_owned(), EdgeKind::Dynamic),
                ("./ref.ts".to_owned(), EdgeKind::Reference),
                ("./side".to_owned(), EdgeKind::Static),
                ("./star".to_owned(), EdgeKind::Static),
                ("./t".to_owned(), EdgeKind::Either),
                ("./u".to_owned(), EdgeKind::Static),
                ("./v".to_owned(), EdgeKind::Static),
                ("./x".to_owned(), EdgeKind::Require),
                ("node".to_owned(), EdgeKind::Types),
            ]
        );
    }

    #[test]
    fn collects_javascript_requires_and_skips_ambient_relative_imports() {
        assert_eq!(
            edges(
                "a.js",
                "const b = require(\"./b\");\nrequire(\"./c\", 1);\n"
            ),
            vec![("./b".to_owned(), EdgeKind::Require)]
        );
        // ambient module の中の相対の指定子は集めない (ambient module の名前は取り込みではない)
        assert_eq!(
            edges(
                "a.d.ts",
                "declare module \"pkg\" {\n  import \"./relative\";\n  import \"other\";\n}\n"
            ),
            vec![("other".to_owned(), EdgeKind::Static)]
        );
        // `export import x = require("…")` は 1 つの文のときだけ (間のコメントは読み飛ばす)。
        // `require` という名前空間の別名と別の式の文、モジュールの拡張は取り込みではない
        assert_eq!(
            edges("a.ts", "export import y = require(\"./y\");\n"),
            vec![("./y".to_owned(), EdgeKind::Require)]
        );
        assert_eq!(
            edges("a.ts", "export import y = require /* c */ (\"./y\");\n"),
            vec![("./y".to_owned(), EdgeKind::Require)]
        );
        assert!(
            edges(
                "a.ts",
                "namespace require { export const x = 1; }\nexport import alias = require;\n(\"./lib\");\n"
            )
            .is_empty()
        );
        assert!(edges("a.ts", "export {};\ndeclare module \"./lib\" {}\n").is_empty());
        // 同じ宣言ファイルで宣言した ambient module の名前の取り込みは、ファイルを探さない
        assert!(
            edges(
                "a.d.ts",
                "declare module \"use\" {\n  export { X } from \"pkg\";\n}\ndeclare module \"pkg\" {\n  export interface X {}\n}\n"
            )
            .is_empty()
        );
        // 本体の無い `declare module "pkg";` も同じ
        assert!(
            edges(
                "a.d.ts",
                "declare module \"pkg\";\ndeclare module \"use\" {\n  import p = require(\"pkg\");\n  export = p;\n}\n"
            )
            .is_empty()
        );
        // JS の JSDoc の import は扱えない
        assert!(
            !extract_imports("a.js", "/** @type {import(\"./t\").T} */\nlet t;\n")
                .expect("imports")
                .complete
        );
    }

    #[test]
    fn references_follow_leading_comments_only() {
        assert_eq!(
            edges(
                "a.ts",
                "#!/usr/bin/env node\n// note\n/* block */\n/// <reference path='b.ts'/>\nlet x = 1;\n/// <reference path=\"c.ts\" />\n"
            ),
            vec![("b.ts".to_owned(), EdgeKind::Reference)]
        );
        // 閉じていない pragma は読まない
        assert!(edges("a.ts", "/// <reference path=\"b.ts\">\n").is_empty());
    }

    #[test]
    fn suffixes_follow_typescript_extension_substitution() {
        let all = Flags {
            typescript: true,
            declaration: true,
            javascript: true,
            json: false,
        };
        assert_eq!(
            suffixes(".js", all, false),
            [".ts", ".tsx", ".d.ts", ".js", ".jsx"]
        );
        assert_eq!(
            suffixes(".jsx", all, false),
            [".tsx", ".ts", ".d.ts", ".jsx", ".js"]
        );
        assert_eq!(suffixes(".mjs", all, false), [".mts", ".d.mts", ".mjs"]);
        assert_eq!(suffixes(".css", all, false), [".d.css.ts"]);
    }

    #[test]
    fn variants_follow_typescript_defaults() {
        let kinds = |module: Option<Module>, target: Option<u32>| {
            [false, true]
                .map(|v6| variant(module, None, target, None, v6).map(|variant| variant.kind))
        };
        assert_eq!(kinds(None, None), [Some(Kind::Node10), Some(Kind::Bundler)]);
        assert_eq!(
            kinds(None, Some(2020)),
            [Some(Kind::Classic), Some(Kind::Bundler)]
        );
        assert_eq!(
            kinds(Some(Module::CommonJs), None),
            [Some(Kind::Node10), Some(Kind::Bundler)]
        );
        assert_eq!(
            kinds(Some(Module::NodeJson), None),
            [Some(Kind::Node16), Some(Kind::Node16)]
        );
        assert_eq!(
            kinds(Some(Module::Legacy), None),
            [Some(Kind::Classic), Some(Kind::Classic)]
        );
        // node16 の方式に node 系でない `module` は TypeScript の誤り
        assert_eq!(
            variant(Some(Module::Es), Some(Kind::Node16), None, None, true),
            None
        );
    }
}
