//! 在庫の基本の集合から外れたファイル (ignore したファイルと隠しファイル) を、1 回の検出で 1 回
//! だけ流して読み、在庫に加えるかを決める材料と、監査の結果を集める (Issue #208)。
//!
//! - どのファイルも、候補の関数名と宣言のファイルを指す名前が、識別子の単語か escape で現れるかを
//!   記録する
//! - TS 系のファイル (`.d.ts` を含む) は解析して、依存の指定子 (import・`export … from`・
//!   `import()` (型の位置を含む)・`require()`・`import x = require()`・`declare module`・
//!   triple-slash の reference) を集める
//! - 監査のために、JSX の宣言・pragma・`declare`・reference の印か、識別子の escape がある
//!   TS / JS 系のファイルを解析して調べる

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use tree_sitter::Node;

use super::repository_audit::{
    ModuleResolution, OutsideSource, PackageBoundary, audit_source, normalize_lexically,
    reference_attribute, reference_attributes, resolve_module,
};
use super::source_scan::{Budget, Needle, Needles, Scan, scan_file};
use crate::commands::dead_code_default_liveness::static_specifier;
use crate::engine::parser::{self, SourceBuf};
use crate::engine::tree_walk::{Children, Walk, walk_preorder};
use crate::language::LangId;

/// 解析する在庫の外のファイル 1 つの上限。
const MAX_PARSED_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// 依存の指定子を持ち得る TS 系のファイルの印。
const DEPENDENCY_MARKERS: [&str; 4] = ["import", "require", "from", "declare"];
/// 監査で調べる印 (大文字小文字を区別するもの)。
const AUDIT_MARKERS: [&str; 2] = ["JSX", "declare"];
/// 監査で調べる印 (大文字小文字を区別しないもの)。`<reference` は依存の印でもある。
const AUDIT_MARKERS_IGNORE_CASE: [&str; 2] = ["@jsx", "<reference"];

/// 在庫の外のファイルを読んで解析する。1 ファイルの上限か、解析する量の残りを超えるとき・
/// 読めないときは None。
pub(super) fn parse_outside(
    path: &Path,
    budget: &Budget,
) -> Option<(SourceBuf, tree_sitter::Tree)> {
    let size = std::fs::metadata(path).ok()?.len();
    if size > MAX_PARSED_FILE_BYTES || !budget.take_parse(size) {
        return None;
    }
    let utf8 = camino::Utf8Path::from_path(path)?;
    let text = parser::read_file(utf8).ok()?;
    let (tree, _) = parser::parse_file(utf8, &text).ok()?;
    Some((text, tree))
}

/// 依存の指定子。
#[derive(Debug, PartialEq, Eq)]
enum Dependency {
    /// 文字列で書いた指定子。
    Specifier(String),
    /// `/// <reference path>` (ファイルのディレクトリからのパス)。
    ReferencePath(String),
    /// `/// <reference types>`。
    ReferenceTypes(String),
    /// 静的に決められない依存 (`import(式)`・`require` の別名・`import.meta`・解析できない
    /// ファイルなど)。
    Unknown,
}

/// TS 系のファイルの依存の指定子を集める。
fn collect_dependencies(root: Node<'_>, source: &[u8]) -> Vec<Dependency> {
    let mut dependencies = Vec::new();
    let mut specifier = |node: Option<Node<'_>>| {
        dependencies.push(
            node.filter(|node| matches!(node.kind(), "string" | "template_string"))
                .and_then(|node| static_specifier(node, source))
                .map_or(Dependency::Unknown, |specifier| {
                    Dependency::Specifier(specifier.to_owned())
                }),
        );
    };
    let mut references = Vec::new();
    let _ = walk_preorder(root, Children::All, |node, _| {
        match node.kind() {
            "import_statement" | "export_statement" => {
                if let Some(source_node) = node.child_by_field_name("source") {
                    specifier(Some(source_node));
                }
            }
            "import_require_clause" => {
                let mut cursor = node.walk();
                let string = node
                    .named_children(&mut cursor)
                    .find(|child| child.kind() == "string");
                specifier(string);
            }
            "call_expression" => {
                let function = node.child_by_field_name("function");
                let loads = function.is_some_and(|function| {
                    function.kind() == "import"
                        || (function.kind() == "identifier"
                            && function.utf8_text(source).ok() == Some("require"))
                });
                if loads {
                    let first = node.child_by_field_name("arguments").and_then(|arguments| {
                        let mut cursor = arguments.walk();
                        arguments
                            .named_children(&mut cursor)
                            .find(|child| child.kind() != "comment")
                    });
                    specifier(first);
                }
            }
            "identifier" if node.utf8_text(source).ok() == Some("require") => {
                let direct = node.parent().is_some_and(|parent| {
                    parent.kind() == "call_expression"
                        && parent
                            .child_by_field_name("function")
                            .is_some_and(|function| function.id() == node.id())
                });
                if !direct {
                    specifier(None);
                }
            }
            "meta_property" => specifier(None),
            "module" | "internal_module" => {
                if let Some(name) = node
                    .child_by_field_name("name")
                    .filter(|name| name.kind() == "string")
                {
                    // `*` を含む名前はパターンの ambient module で、ファイルに解決される
                    // モジュールを変えない。
                    if !static_specifier(name, source).is_some_and(|name| name.contains('*')) {
                        specifier(Some(name));
                    }
                }
            }
            "comment" => {
                let text = String::from_utf8_lossy(&source[node.byte_range()]);
                if let Some(attributes) = reference_attributes(&text) {
                    let value = |name: &str| {
                        reference_attribute(attributes, name)
                            .filter(|value| !value.is_empty())
                            .map(str::to_owned)
                    };
                    if let Some(types) = value("types") {
                        references.push(Dependency::ReferenceTypes(types));
                    } else if let Some(path) = value("path") {
                        references.push(Dependency::ReferencePath(path));
                    }
                }
            }
            _ => {}
        }
        ControlFlow::<(), Walk>::Continue(Walk::Descend)
    });
    dependencies.extend(references);
    dependencies
}

/// `.mts`・`.cts` か、トップレベルに import / export 文がある (module である)。
fn is_module(relative: &str, root: Node<'_>) -> bool {
    let module_extension = Path::new(relative)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("mts") || extension.eq_ignore_ascii_case("cts")
        });
    let mut cursor = root.walk();
    module_extension
        || root
            .named_children(&mut cursor)
            .any(|statement| matches!(statement.kind(), "import_statement" | "export_statement"))
}

/// 宣言のファイルを指す指定子に現れる名前。拡張子より前のファイル名で、`index` ならディレクトリの
/// 名前 (ディレクトリが無ければ `index`)。
pub(super) fn module_word(target: &str) -> Option<&str> {
    let mut segments = target.rsplit('/');
    let file = segments.next()?;
    let stem = file.split('.').next()?;
    let word = if stem == "index" {
        segments.next().unwrap_or(stem)
    } else {
        stem
    };
    (!word.is_empty()).then_some(word)
}

/// 利用の式を持ち得る TS 系のファイルか (`.ts`・`.tsx`・`.mts`・`.cts`。宣言の `.d.ts` 系を除く)。
fn ts_expression_source(source: &OutsideSource) -> bool {
    let lower = source.relative.to_ascii_lowercase();
    matches!(source.lang, Some(LangId::Typescript | LangId::Tsx))
        && ![".d.ts", ".d.mts", ".d.cts"]
            .iter()
            .any(|suffix| lower.ends_with(suffix))
}

/// 在庫の外のファイル 1 つの事実。
struct OutsideFile {
    source: OutsideSource,
    scan: Scan,
    /// TS 系のファイルの依存の指定子。
    dependencies: Vec<Dependency>,
    /// 大域の宣言を持ち得る TS 系のファイル (module でないか `global` を含む。解析しなかったものを
    /// 含む)。
    global_capable: bool,
}

/// 在庫の外のファイルを 1 回流して読んだ結果。
pub(super) struct OutsideScan {
    root: PathBuf,
    /// 照合した語 (候補の関数名と、宣言のファイルを指す名前)。番号は needle の番号と同じ。
    words: Vec<String>,
    files: Vec<OutsideFile>,
    /// 走査で照合しなかった語の、ファイルごとの出現。None は読めなかった・上限を超えた。
    later_words: RefCell<HashMap<String, Option<Vec<bool>>>>,
    /// 外のファイルが JSX の型環境を差し替えていないか。監査できなかったら None。
    pub(super) jsx_standard: Option<bool>,
}

impl OutsideScan {
    /// `sources` を流して読む。読めないファイルがある・読む量か解析する量の残りを超えるときは
    /// None (どの証明もしない)。`packages` が None なら監査はしない (`jsx_standard` は None)。
    pub(super) fn scan(
        root: &Path,
        sources: Vec<OutsideSource>,
        words: Vec<String>,
        packages: Option<&PackageBoundary>,
        budget: &Budget,
    ) -> Option<Self> {
        let marker_start = words.len();
        let needles = Needles::new(
            words
                .iter()
                .map(|word| Needle::word(word))
                .chain(DEPENDENCY_MARKERS.iter().map(|marker| Needle::text(marker)))
                .chain(AUDIT_MARKERS.iter().map(|marker| Needle::text(marker)))
                .chain(
                    AUDIT_MARKERS_IGNORE_CASE
                        .iter()
                        .map(|marker| Needle::ignore_case(marker)),
                )
                .collect(),
        );
        let dependency_range = marker_start..marker_start + DEPENDENCY_MARKERS.len();
        let audit_range = dependency_range.end..needles.len();
        let mut jsx_standard = packages.map(|_| true);
        let mut files = Vec::new();
        for source in sources {
            let scan = scan_file(&source.path, &needles, budget)?;
            let mut dependencies = Vec::new();
            let mut global_capable = false;
            if let Some(lang) = source.lang {
                let ts = matches!(lang, LangId::Typescript | LangId::Tsx);
                global_capable = ts;
                let dependency_marked = dependency_range.clone().any(|index| scan.found(index))
                    || scan.found(needles.len() - 1);
                let audit_marked =
                    audit_range.clone().any(|index| scan.found(index)) || scan.escapes_identifier();
                if (ts && dependency_marked) || audit_marked {
                    let (text, tree) = parse_outside(&source.path, budget)?;
                    let tree_root = tree.root_node();
                    if tree_root.has_error() {
                        jsx_standard = None;
                        dependencies.push(Dependency::Unknown);
                    } else {
                        if ts {
                            dependencies = collect_dependencies(tree_root, text.as_ref());
                            global_capable = !is_module(&source.relative, tree_root)
                                || memchr::memmem::find(text.as_ref(), b"global").is_some();
                        }
                        if let (Some(standard), Some(packages)) = (jsx_standard, packages) {
                            jsx_standard = audit_source(
                                root,
                                &source.relative,
                                text.as_ref(),
                                tree_root,
                                packages,
                            )
                            .map(|outside| standard && outside);
                        }
                    }
                }
            }
            files.push(OutsideFile {
                source,
                scan,
                dependencies,
                global_capable,
            });
        }
        Some(Self {
            root: root.to_path_buf(),
            words,
            files,
            later_words: RefCell::new(HashMap::new()),
            jsx_standard,
        })
    }

    /// 在庫の外のソース (テスト用)。
    #[cfg(test)]
    pub(super) fn sources(&self) -> impl Iterator<Item = &OutsideSource> {
        self.files.iter().map(|file| &file.source)
    }

    /// 大域の宣言を持ち得る TS 系のファイル。
    pub(super) fn global_sources(&self) -> impl Iterator<Item = &OutsideSource> {
        self.files
            .iter()
            .filter(|file| file.global_capable)
            .map(|file| &file.source)
    }

    /// ファイルごとの `word` の出現 (escape を含む)。走査で照合しなかった語は、もう一度流して
    /// 読む (読む量は検出の全体で共有する)。読めないときは None。
    fn mentions(&self, word: &str, budget: &Budget) -> Option<Vec<bool>> {
        if let Some(index) = self.words.iter().position(|scanned| scanned == word) {
            return Some(
                self.files
                    .iter()
                    .map(|file| file.scan.found(index) || file.scan.escapes_any(word))
                    .collect(),
            );
        }
        if let Some(found) = self.later_words.borrow().get(word) {
            return found.clone();
        }
        let needles = Needles::new(vec![Needle::word(word)]);
        let found = self
            .files
            .iter()
            .map(|file| {
                scan_file(&file.source.path, &needles, budget)
                    .map(|scan| scan.found_any() || file.scan.escapes_any(word))
            })
            .collect::<Option<Vec<bool>>>();
        self.later_words
            .borrow_mut()
            .insert(word.to_owned(), found.clone());
        found
    }

    /// 宣言のファイル `target` にある `name` のために、在庫に加えるファイル。名前 (宣言でない
    /// TS 系では宣言のファイルを指す名前も) が現れるファイルと、宣言のファイルに解決し得るか解決先を
    /// 決められない依存を持つ TS 系のファイル。埋め込みのソースに名前が現れる・読めないときは None。
    pub(super) fn selected(
        &self,
        name: &str,
        target: &str,
        packages: Option<&PackageBoundary>,
        budget: &Budget,
    ) -> Option<Vec<&OutsideSource>> {
        let names = self.mentions(name, budget)?;
        let module = module_word(target).filter(|module| *module != name);
        let modules = match module {
            Some(module) => Some(self.mentions(module, budget)?),
            None => None,
        };
        let mut selected = Vec::new();
        for (index, file) in self.files.iter().enumerate() {
            let source = &file.source;
            let mentioned = names[index]
                || (ts_expression_source(source)
                    && modules.as_ref().is_some_and(|modules| modules[index]));
            if mentioned {
                // 埋め込みのソースの中の利用は、構文を解析できないので証明しない。
                source.lang?;
                selected.push(source);
                continue;
            }
            let depends = file.dependencies.iter().any(|dependency| {
                may_reach(dependency, &self.root, &source.relative, target, packages)
            });
            if depends {
                selected.push(source);
            }
        }
        Some(selected)
    }
}

/// 依存が宣言のファイル `target` に解決され得るか。相対の指定子は、ファイル (拡張子の補完を
/// 含む)・ディレクトリの index の順に実際のファイルで解く。ディレクトリに package.json がある
/// (types・main・exports で何でも指せる) か、何も無ければ決められない。bare specifier は
/// `paths`・`baseUrl`・リポジトリの package で解く。registry の依存と決められず解決先も確かめ
/// られない依存も true。
fn may_reach(
    dependency: &Dependency,
    root: &Path,
    importer: &str,
    target: &str,
    packages: Option<&PackageBoundary>,
) -> bool {
    let relative = |specifier: &str| {
        let directory = matches!(specifier.rsplit('/').next(), Some("" | "." | ".."));
        let importer_directory = Path::new(importer).parent().unwrap_or(Path::new(""));
        // 途中で解析するディレクトリの外へ出て戻る指定子もあるので、最後の位置で決める。
        let Some(joined) = normalize_lexically(&root.join(importer_directory).join(specifier))
        else {
            return false;
        };
        // 解析するディレクトリの外のモジュールは、宣言のファイルではない。
        joined.starts_with(root)
            && !matches!(
                resolve_module(root, &joined, directory, target),
                ModuleResolution::Elsewhere
            )
    };
    let bare = |specifier: &str| {
        packages.is_none_or(|packages| packages.bare_may_reach(specifier, target))
    };
    match dependency {
        Dependency::Unknown => true,
        Dependency::Specifier(specifier) => {
            if specifier.starts_with("./")
                || specifier.starts_with("../")
                || matches!(specifier.as_str(), "." | "..")
            {
                relative(specifier)
            } else if specifier.starts_with('/') {
                true
            } else {
                bare(specifier)
            }
        }
        Dependency::ReferencePath(path) => {
            if path.starts_with('/') || path.contains('\\') {
                true
            } else if path.starts_with("./") || path.starts_with("../") {
                relative(path)
            } else {
                relative(&format!("./{path}"))
            }
        }
        Dependency::ReferenceTypes(types) => {
            if types.starts_with("./") || types.starts_with("../") {
                relative(types)
            } else if types.starts_with('/') {
                true
            } else {
                bare(types)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::parser::parse_source;

    #[test]
    fn outside_dependencies_cover_every_loading_form() {
        let source = br#"/// <reference path="../src/panel.tsx" />
/// <reference types="node" />
import * as m from "../src/panel";
import type { T } from "./types";
export * from "./barrel";
export { x } from "pkg/sub";
type A = typeof import("../src/a");
type B = import("../src/b").Props;
const c = import("./c");
import d = require("./d");
declare module "./e" {}
declare module "*.svg" {}
const f = require(name);
const g = require;
const h = import.meta.url;
const i = import(`./${name}`);
"#;
        let tree = parse_source(source, LangId::Typescript).unwrap();
        let dependencies = collect_dependencies(tree.root_node(), source);
        let specifier = |text: &str| Dependency::Specifier(text.to_owned());
        assert_eq!(
            dependencies,
            [
                specifier("../src/panel"),
                specifier("./types"),
                specifier("./barrel"),
                specifier("pkg/sub"),
                specifier("../src/a"),
                specifier("../src/b"),
                specifier("./c"),
                specifier("./d"),
                specifier("./e"),
                Dependency::Unknown,
                Dependency::Unknown,
                Dependency::Unknown,
                Dependency::Unknown,
                Dependency::ReferencePath("../src/panel.tsx".to_owned()),
                Dependency::ReferenceTypes("node".to_owned()),
            ]
        );
        assert_eq!(module_word("src/panel.tsx"), Some("panel"));
        assert_eq!(module_word("src/panel/index.tsx"), Some("panel"));
        assert_eq!(module_word("index.ts"), Some("index"));
        assert_eq!(module_word("src/panel.view.tsx"), Some("panel"));
    }

    #[test]
    fn outside_dependencies_reach_the_target_by_relative_paths() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        for (path, source) in [
            ("src/index.tsx", "export {};\n"),
            ("src/other.ts", "export {};\n"),
            ("lib/index.ts", "export {};\n"),
            ("ui/package.json", "{ \"types\": \"./components.tsx\" }\n"),
            ("ui/components.tsx", "export {};\n"),
            ("styles.css", "a {}\n"),
        ] {
            std::fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
            std::fs::write(root.join(path), source).unwrap();
        }
        let reach = |dependency: Dependency, importer: &str| {
            may_reach(&dependency, &root, importer, "src/index.tsx", None)
        };
        let specifier = |text: &str| Dependency::Specifier(text.to_owned());
        // ファイル・ディレクトリの index・`.js` の読み替えで、宣言のファイルに当たる。
        assert!(reach(specifier("."), "src/ignored-use.ts"));
        assert!(reach(specifier("../src"), "generated/x.ts"));
        assert!(reach(specifier("../src/index.js"), "generated/x.ts"));
        assert!(reach(
            Dependency::ReferencePath("index.tsx".to_owned()),
            "src/types.d.ts"
        ));
        // ほかのファイルに解決されるものは届かない。
        assert!(!reach(specifier("./other"), "src/ignored-use.ts"));
        assert!(!reach(specifier("../lib"), "src/x.ts"));
        assert!(!reach(specifier("../styles.css"), "src/x.ts"));
        assert!(!reach(specifier("../../outside"), "src/x.ts"));
        // 解析するディレクトリの外へ一度出て戻る指定子は、最後の位置で決める。
        let name = root.file_name().unwrap().to_str().unwrap().to_owned();
        assert!(reach(
            specifier(&format!("../../{name}/src/index")),
            "generated/exports.d.ts"
        ));
        assert!(!reach(
            specifier(&format!("../../{name}/src/other")),
            "generated/exports.d.ts"
        ));
        // package.json のあるディレクトリ (types・main・exports で何でも指せる) と、何も無い
        // 指定子は決められない。
        assert!(reach(specifier("../ui"), "generated/gallery.tsx"));
        assert!(reach(specifier("./missing"), "src/x.ts"));
        // bare specifier は、依存の判定の材料が無ければ解決先を決められない。
        assert!(reach(specifier("react"), "generated/x.ts"));
        assert!(reach(Dependency::Unknown, "generated/x.ts"));
    }
}
