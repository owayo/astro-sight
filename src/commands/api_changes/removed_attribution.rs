//! api.rm 候補の参照帰属解決 (Issue 2026-07-19-bulk-subsystem-removal)。
//!
//! `partition_removed_dead_candidates` の bare name 参照カウントは、削除シンボルと同名の
//! 残存シンボル (別ファイルの同名 shell 関数や同名 export) への参照を「削除シンボルへの
//! 残存参照」と誤認し、意図的な bulk removal を blocking (api.rm) に残す。参照 1 件ごとに
//! 「残存シンボル由来である」ことの証明を試み、全参照が証明できた候補だけを removed_dead
//! (informational) へ降格する。証明できない参照は従来どおり残存参照として数える
//! (fail-closed、破壊的削除の見逃しを作らない)。

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use super::removed_ffi::ForeignBindability;
use crate::engine::parser;
use crate::language::LangId;

/// 参照 1 件の帰属解決結果。candidate (削除元 old_path) 非依存の事実を保持し、
/// `proves_survivor_origin` で old_path / 残存定義ファイル集合と照合する。
#[derive(Clone)]
pub(crate) struct RefAttribution {
    /// 参照ファイルの言語。解決できない場合は None (言語による証明はしない = fail-closed)。
    pub(crate) ref_lang: Option<LangId>,
    /// 参照ファイル (dir 相対)。
    pub(crate) ref_path: String,
    /// Rust の出現が、同じファイル直下の同名の自由関数か、より内側の束縛に解決する位置か
    /// (`removed_scope::rust_file_scope_positions`)。
    pub(crate) rust_binds_in_file: bool,
    pub(crate) origin: RefOrigin,
}

/// 参照が「残存シンボル由来」であることの根拠。
#[derive(Clone)]
pub(crate) enum RefOrigin {
    /// 通常の自由関数定義、または同じ Python モジュール内での読み取り。Rust では、修飾部や
    /// 素の名前から解決できた残るモジュールの自由関数 (path は宣言したファイル) も含む。
    IndependentDefinition { path: String },
    /// Rust のローカル値束縛・非 callable なフィールド名位置。
    LocalValue,
    /// Rust の関連メンバー定義、直接のメソッド呼び出し、または型を修飾部に持つ関連項目のパス。
    RustAssociatedMember,
    /// 参照ファイル自身に同名定義が残存している (bash)。静的な依存先に削除ファイルが
    /// 含まれる場合は証明しない。
    SelfDefined {
        /// 静的な依存先の解決候補 (repo 相対)。
        sourced_candidates: Option<Rc<HashSet<String>>>,
    },
    /// TS/JS: 参照位置の識別子が、字句スコープ上で同じファイルの通常の宣言 (関数・クラス・型・
    /// 独立した初期化子の const) に束縛される。JS/TS のメソッドの削除はこの証拠では降格しない。
    /// 静的な依存先に削除ファイルが含まれる場合と、依存先を静的に決められない (`import()` に
    /// 置換付きのテンプレート) 場合も証明しない。
    JsLocalDeclaration {
        dependency_targets: Rc<HashSet<String>>,
        dependency_opaque: bool,
    },
    /// TS/JS: 参照が相対 import / from 句付き export の束縛 (文の中の出現、またはその束縛の
    /// 利用) で、文の解決候補が `candidates`。`export_proven` は、存在する全ての候補がその
    /// 公開名を由来の分かる形 (通常の宣言、または同名の明示的な再 export) で export していること。
    ImportResolved {
        candidates: Rc<HashSet<String>>,
        export_proven: bool,
    },
    /// TS/JS: 削除シンボルを指し得ない位置 (オブジェクトのキー、フィールド名、型のプロパティ名、
    /// JSX の属性名、引数などの束縛の宣言名) と、外部パッケージの束縛の利用。
    NonUse,
    /// Receiver is an instance of a directly imported, surviving class.
    JsMemberOfResolvedClass(Rc<super::js_member_origin::JsMemberEvidence>),
    /// Python: symbol の出現がすべて属性アクセス (`re.search` / `self.search`) で、
    /// レシーバの実効モジュール名が `receiver_module_names`。モジュールレベルの自由関数 /
    /// クラスは `<module>.<name>` (または再エクスポートした祖先パッケージの
    /// `<package>.<name>`) 形式でしか属性アクセス経由に到達できないため、レシーバ名の中に
    /// 削除モジュール名とその祖先パッケージ名が無ければ残存シンボル由来と証明できる。
    PythonAttributeAccess {
        receiver_module_names: Rc<HashSet<String>>,
    },
    /// 証明不能。従来どおり残存参照として数える。
    Unproven,
}

/// `proves_survivor_origin` の照合対象となる削除候補。同型 `&str` の `old_path` / `name` /
/// `kind` を位置引数で渡すと取り違えてもコンパイルが通るため構造体で束ねる。
pub(crate) struct RemovedCandidateRef<'a> {
    /// 削除元ファイル (repo 相対)。
    pub(crate) old_path: &'a str,
    /// 削除シンボル名。qualname (`BM25.fit`) はメソッドを表す。
    pub(crate) name: &'a str,
    /// 削除シンボルの kind (`function` / `method` / `class` ...)。
    pub(crate) kind: &'a str,
    pub(super) rust_module_function: Option<super::rust_ref_sites::ModuleFunctionProof>,
}

/// `attr` が「削除ファイル `candidate.old_path` のシンボルではなく残存シンボル由来」と
/// 証明できるか (旧ツリーの FFI 痕跡は調べず、言語表だけで判定する)。
#[cfg(test)]
pub(crate) fn proves_survivor_origin(
    attr: &RefAttribution,
    candidate: &RemovedCandidateRef<'_>,
) -> bool {
    proves_survivor_origin_with_ffi(attr, candidate, ForeignBindability::ANY)
}

/// 参照側の言語から削除側の言語へ識別子束縛の経路が無いと言えるか。
///
/// 削除側が Rust で参照側が Swift / JS / TS の場合は、言語表ではなく旧ツリーの痕跡
/// (`foreign`) で決める。wasm-bindgen / napi-rs / neon は生成した JS モジュールから Rust の
/// 関数を同じ識別子で公開し、C ABI の関数も wasm の export として同じ名前で呼べる。言語表
/// (`can_reference_definition_in`) だけで JS → Rust を「束縛できない」と断定すると、JS から
/// 呼ばれている Rust 関数の削除が removed_dead に降格する。
pub(crate) fn cross_language_unbindable(
    ref_lang: Option<LangId>,
    candidate: &RemovedCandidateRef<'_>,
    foreign: ForeignBindability,
) -> bool {
    let (Some(reference), Some(old)) = (ref_lang, path_lang(candidate.old_path)) else {
        return false;
    };
    if old == LangId::Rust && ForeignBindability::is_evidence_consumer(reference) {
        return !foreign.allows(reference);
    }
    !reference.can_reference_definition_in(old)
}

pub(crate) fn proves_survivor_origin_with_ffi(
    attr: &RefAttribution,
    candidate: &RemovedCandidateRef<'_>,
    foreign_bindable: ForeignBindability,
) -> bool {
    // 参照ファイルの言語から削除ファイルの言語へ識別子束縛の経路が無いなら、同名でも
    // 別物と断言できる。polyglot リポジトリでは汎用名 (`search` / `init`) の bare name
    // 参照の大半がこれに当たる (Issue 2026-08-06-api-rm-atomic-module-deletion では
    // 2,522 件中 2,521 件が Python 削除に対する PHP / C / JS / TS の同名参照だった)。
    if cross_language_unbindable(attr.ref_lang, candidate, foreign_bindable) {
        return true;
    }
    match &attr.origin {
        RefOrigin::IndependentDefinition { path } => {
            candidate.kind == "function"
                && !candidate.name.contains('.')
                && super::removed_scope::module_key(path)
                    != super::removed_scope::module_key(candidate.old_path)
        }
        RefOrigin::LocalValue => candidate.kind == "function" && !candidate.name.contains('.'),
        RefOrigin::RustAssociatedMember => {
            attr.ref_lang == Some(LangId::Rust) && candidate.rust_module_function.is_some()
        }
        RefOrigin::SelfDefined { sourced_candidates } => {
            sourced_candidates
                .as_ref()
                .is_none_or(|sourced| !sourced.contains(candidate.old_path))
                && !(matches!(candidate.kind, "method" | "function")
                    && (candidate.kind == "method" || candidate.name.contains('.'))
                    && path_lang(candidate.old_path).is_some_and(is_js_ts_family))
        }
        RefOrigin::JsLocalDeclaration {
            dependency_targets,
            dependency_opaque,
        } => {
            !dependency_opaque
                && !dependency_targets.contains(candidate.old_path)
                && !(matches!(candidate.kind, "method" | "function")
                    && (candidate.kind == "method" || candidate.name.contains('.'))
                    && path_lang(candidate.old_path).is_some_and(is_js_ts_family))
        }
        RefOrigin::NonUse => true,
        RefOrigin::JsMemberOfResolvedClass(evidence) => {
            matches!(candidate.kind, "method" | "function")
                && path_lang(candidate.old_path).is_some_and(is_js_ts_family)
                && super::js_member_origin::is_normalized_repo_path(candidate.old_path)
                && !evidence
                    .class_files
                    .iter()
                    .any(|path| super::js_member_origin::paths_match(path, candidate.old_path))
                && !evidence
                    .dependency_targets
                    .iter()
                    .any(|path| super::js_member_origin::paths_match(path, candidate.old_path))
        }
        RefOrigin::ImportResolved {
            candidates,
            export_proven,
        } => *export_proven && !candidates.contains(candidate.old_path),
        RefOrigin::PythonAttributeAccess {
            receiver_module_names,
        } => {
            // メソッド削除 (`BM25.fit`) では属性アクセスこそが正当な参照形なので証明しない。
            // qualname でなくとも kind が method なら同様に fail-closed に倒す。
            if candidate.kind == "method" || candidate.name.contains('.') {
                return false;
            }
            let modules = python_module_names(candidate.old_path);
            !modules.is_empty()
                && !modules
                    .iter()
                    .any(|module| receiver_module_names.contains(module))
        }
        RefOrigin::Unproven => false,
    }
}

/// 定義数ガードの緩和では、既存のファイル単位 SelfDefined を証明に使わない。
pub(crate) fn proves_independent_definition_with_ffi(
    attr: &RefAttribution,
    candidate: &RemovedCandidateRef<'_>,
    foreign_bindable: ForeignBindability,
) -> bool {
    let cross_language = cross_language_unbindable(attr.ref_lang, candidate, foreign_bindable);
    (cross_language
        || matches!(
            &attr.origin,
            RefOrigin::IndependentDefinition { .. }
                | RefOrigin::LocalValue
                | RefOrigin::RustAssociatedMember
        ))
        && proves_survivor_origin_with_ffi(attr, candidate, foreign_bindable)
}

/// 削除元 Python ファイルのシンボルへ属性アクセスで到達しうるモジュール名。
/// `pkg/core.py` → `core` / `pkg`、`pkg/__init__.py` → `pkg` (パッケージ名で参照されるため)。
///
/// 祖先ディレクトリ名も含めるのは、`pkg/__init__.py` の `from .core import search` のような
/// 再エクスポートで `pkg.search` としても到達できるため。祖先が実際に再エクスポートして
/// いるかは確かめず、すべて照合対象にする (証明を諦める側 = blocking 維持に倒れる)。
fn python_module_names(old_path: &str) -> Vec<String> {
    let path = std::path::Path::new(old_path);
    let mut names: Vec<String> = Vec::new();
    if let Some(stem) = path.file_stem().and_then(|s| s.to_str())
        && stem != "__init__"
    {
        names.push(stem.to_string());
    }
    let mut ancestor = path.parent();
    while let Some(dir) = ancestor {
        if let Some(name) = dir.file_name().and_then(|n| n.to_str()) {
            names.push(name.to_string());
        }
        ancestor = dir.parent();
    }
    names
}

/// (ref_path, symbol) 単位のファイル解析結果。参照ループでキャッシュされ、
/// `attribution_for_ref` で `RefAttribution` に変換される。
pub(crate) struct RefAttributionFacts {
    /// 0 始まりの行・byte 列。解析一回の寿命に閉じ、別 revision へ再利用しない。
    independent_positions: super::removed_scope::Positions,
    local_positions: super::removed_scope::Positions,
    rust_file_scope_positions: super::removed_scope::Positions,
    rust_sites: HashMap<(usize, usize), super::rust_ref_sites::MemberSite>,
    rust_paths: HashMap<(usize, usize), super::removed_rust_paths::PathSite>,
    /// JS/TS: 出現の位置 (0 始まりの行・byte 列) ごとの分類。外部パッケージの束縛
    /// (`JsRefSite::External`) の出現は参照に数えない。
    js_sites: HashMap<(usize, usize), super::js_ref_sites::JsRefSite>,
    pub js_member_evidence: HashMap<(usize, usize), Rc<super::js_member_origin::JsMemberEvidence>>,
    /// bash のみ Some: リテラル source の解決候補。
    pub bash_sourced_candidates: Option<Rc<HashSet<String>>>,
    /// JS/TS のみ Some: ファイル内の静的な相対依存先。
    pub js_dependency_targets: Option<Rc<HashSet<String>>>,
    /// JS/TS: 依存先を静的に決められない文字列 (置換付きのテンプレート) がある。
    js_dependency_opaque: bool,
    /// Python のみ Some: symbol の非定義出現がすべて属性アクセスだった場合の、
    /// レシーバの実効モジュール名集合。1 件でも bare identifier 出現があれば None。
    pub python_attribute_receivers: Option<Rc<HashSet<String>>>,
    /// 参照ファイルの言語。解決できなければ None。
    pub ref_lang: Option<LangId>,
    /// ファイルが TS/JS 系か。
    pub is_js_ts: bool,
    /// ファイルが bash 系か。
    pub is_bash: bool,
}

impl RefAttributionFacts {
    fn opaque() -> Self {
        Self {
            independent_positions: HashSet::new(),
            local_positions: HashSet::new(),
            rust_file_scope_positions: HashSet::new(),
            rust_sites: HashMap::new(),
            rust_paths: HashMap::new(),
            js_sites: HashMap::new(),
            js_member_evidence: HashMap::new(),
            bash_sourced_candidates: None,
            js_dependency_targets: None,
            js_dependency_opaque: false,
            python_attribute_receivers: None,
            ref_lang: None,
            is_js_ts: false,
            is_bash: false,
        }
    }

    /// この位置の出現が外部パッケージの import とその束縛の利用か (削除したローカルの
    /// シンボルとは別物なので参照に数えない)。
    pub(crate) fn is_external_reference(&self, position: (usize, usize)) -> bool {
        matches!(
            self.js_sites.get(&position),
            Some(super::js_ref_sites::JsRefSite::External)
        )
    }
}

/// facts と残存定義ファイル集合から参照 1 件の帰属を分類する。
/// `ref_path` は参照が見つかったファイル (repo 相対)。
pub(crate) fn attribution_for_ref(
    facts: &RefAttributionFacts,
    ref_path: &str,
    residual_def_paths: &HashSet<String>,
    position: (usize, usize),
) -> RefAttribution {
    RefAttribution {
        ref_lang: facts.ref_lang,
        ref_path: ref_path.to_owned(),
        rust_binds_in_file: facts.rust_file_scope_positions.contains(&position),
        origin: ref_origin_for(facts, ref_path, residual_def_paths, position),
    }
}

fn ref_origin_for(
    facts: &RefAttributionFacts,
    ref_path: &str,
    residual_def_paths: &HashSet<String>,
    position: (usize, usize),
) -> RefOrigin {
    match facts.rust_sites.get(&position) {
        Some(super::rust_ref_sites::MemberSite::UnprovenFieldValue) => return RefOrigin::Unproven,
        Some(
            super::rust_ref_sites::MemberSite::AssociatedDefinition
            | super::rust_ref_sites::MemberSite::DirectMethodCall,
        ) => return RefOrigin::RustAssociatedMember,
        None => {}
    }
    match facts.rust_paths.get(&position) {
        Some(super::removed_rust_paths::PathSite::Associated) => {
            return RefOrigin::RustAssociatedMember;
        }
        Some(super::removed_rust_paths::PathSite::ModuleFunction(path)) => {
            return RefOrigin::IndependentDefinition {
                path: path.to_string(),
            };
        }
        None => {}
    }
    if facts.local_positions.contains(&position) {
        return RefOrigin::LocalValue;
    }
    if facts.independent_positions.contains(&position) {
        return RefOrigin::IndependentDefinition {
            path: ref_path.to_string(),
        };
    }
    if facts.is_js_ts {
        use super::js_ref_sites::JsRefSite;
        if let Some(evidence) = facts.js_member_evidence.get(&position) {
            return RefOrigin::JsMemberOfResolvedClass(Rc::clone(evidence));
        }
        // 出現ごとの構文上の位置と字句束縛で帰属を決める。ファイル単位の事実 (同名定義の有無・
        // 同名 import の有無) を全出現へ当てると、`api.fetchUser` のメンバー名や名前空間経由の
        // `a.helper()` まで同名の残存定義に帰属させてしまう。
        return match facts.js_sites.get(&position) {
            Some(JsRefSite::NonUse | JsRefSite::External) => RefOrigin::NonUse,
            Some(JsRefSite::Import {
                candidates,
                export_proven,
            }) => RefOrigin::ImportResolved {
                candidates: Rc::clone(candidates),
                export_proven: *export_proven,
            },
            Some(JsRefSite::LocalDeclaration) => match &facts.js_dependency_targets {
                Some(targets) => RefOrigin::JsLocalDeclaration {
                    dependency_targets: Rc::clone(targets),
                    dependency_opaque: facts.js_dependency_opaque,
                },
                None => RefOrigin::Unproven,
            },
            Some(JsRefSite::Member | JsRefSite::Unproven) | None => RefOrigin::Unproven,
        };
    }
    if facts.is_bash && residual_def_paths.contains(ref_path) {
        return RefOrigin::SelfDefined {
            sourced_candidates: facts.bash_sourced_candidates.clone(),
        };
    }
    if let Some(receivers) = &facts.python_attribute_receivers {
        return RefOrigin::PythonAttributeAccess {
            receiver_module_names: Rc::clone(receivers),
        };
    }
    RefOrigin::Unproven
}

/// `ref_path` のファイルを 1 回だけ parse し、外部 import 事実 (従来) と参照帰属の素材を
/// まとめて返す。非対応言語 / 読み込み・parse 失敗は「何も証明できない」facts を返す
/// (従来どおりカウントされる、保守的)。
pub(crate) fn analyze_ref_attribution_facts(
    dir: &str,
    ref_path: &str,
    symbol: &str,
    external_pkgs: &HashSet<String>,
    member_proof: bool,
    export_cache: &mut super::js_ref_sites::JsExportOriginCache,
    rust_paths: &mut super::removed_rust_paths::RustPathResolver,
) -> RefAttributionFacts {
    let abs = if std::path::Path::new(ref_path).is_absolute() {
        std::path::PathBuf::from(ref_path)
    } else {
        std::path::Path::new(dir).join(ref_path)
    };
    let Some(utf8) = camino::Utf8Path::from_path(&abs) else {
        return RefAttributionFacts::opaque();
    };
    let Some(lang) = file_lang(utf8) else {
        return RefAttributionFacts::opaque();
    };
    let mut facts = match lang {
        LangId::Javascript | LangId::Typescript | LangId::Tsx => analyze_js_ts_facts(
            utf8,
            lang,
            ref_path,
            symbol,
            external_pkgs,
            member_proof,
            dir,
            export_cache,
        ),
        LangId::Bash => analyze_bash_facts(utf8, ref_path),
        LangId::Python => analyze_python_facts(utf8, symbol),
        LangId::Rust => analyze_rust_facts(utf8, ref_path, symbol, rust_paths),
        _ => RefAttributionFacts::opaque(),
    };
    facts.ref_lang = Some(lang);
    facts
}

/// 参照ファイルの言語。拡張子で決まらない場合は shebang まで見る
/// (`scripts/install_prereq` のような拡張子なしシェルスクリプトも参照元になり得る)。
fn file_lang(utf8: &camino::Utf8Path) -> Option<LangId> {
    if let Ok(lang) = LangId::from_path(utf8) {
        return Some(lang);
    }
    // Angular テンプレート。astro-sight が `.html` から参照を出すのは component template
    // 走査経路だけで、その識別子は TS component クラスのメンバに解決される。
    if matches!(
        utf8.extension().map(str::to_ascii_lowercase).as_deref(),
        Some("html") | Some("htm")
    ) {
        return Some(LangId::Typescript);
    }
    let source = parser::read_file(utf8).ok()?;
    let first_line = source.split(|b| *b == b'\n').next()?;
    LangId::from_shebang(std::str::from_utf8(first_line).ok()?)
}

/// 削除元ファイルの言語。削除済みで実体を読めないため拡張子だけで判定する。
pub(super) fn path_lang(path: &str) -> Option<LangId> {
    LangId::from_path(camino::Utf8Path::new(path)).ok()
}

pub(super) fn is_js_ts_family(lang: LangId) -> bool {
    matches!(lang, LangId::Javascript | LangId::Typescript | LangId::Tsx)
}

/// Python の参照ファイルを解析し、symbol の非定義出現がすべて属性アクセス
/// (`re.search` / `self.search` / `pkg.mod.search`) かどうかを判定する。
///
/// すべて属性アクセスなら、レシーバの「実効モジュール名」集合を返す。実効モジュール名は
/// レシーバ式の末尾識別子を、そのファイルの `as` 別名 (`import pkg.core as c` / `from pkg
/// import core as c`) で解決したもの。モジュールレベルの自由関数・クラスへ属性アクセスで
/// 到達する経路は `<module>.<name>` (と再エクスポートした祖先パッケージの `<package>.<name>`)
/// だけなので、この集合に削除モジュール名と祖先パッケージ名が無ければ削除シンボル由来では
/// ないと言える。
///
/// bare identifier としての出現が 1 件でもあれば None を返し従来どおり残存参照として数える
/// (`from core import search` の import 行や `search()` の直接呼び出しがこれに当たる)。
fn analyze_python_facts(utf8: &camino::Utf8Path, symbol: &str) -> RefAttributionFacts {
    let Ok(source) = parser::read_file(utf8) else {
        return RefAttributionFacts::opaque();
    };
    let Ok(tree) = parser::parse_source(&source, LangId::Python) else {
        return RefAttributionFacts::opaque();
    };
    let root = tree.root_node();
    let mut facts = RefAttributionFacts {
        independent_positions: super::removed_scope::python_independent_positions(
            root, &source, symbol,
        ),
        ..RefAttributionFacts::opaque()
    };
    let aliases = collect_python_import_aliases(root, &source);
    let mut receivers: HashSet<String> = HashSet::new();
    let mut import_only = HashMap::new();
    if !collect_python_attribute_receivers(
        root,
        &source,
        symbol,
        &aliases,
        &mut import_only,
        &mut receivers,
    ) {
        return facts;
    }
    facts.python_attribute_receivers = Some(Rc::new(receivers));
    facts
}

/// 同じファイル・解析木で定義座標と値束縛座標をまとめて収集する。
fn analyze_rust_facts(
    utf8: &camino::Utf8Path,
    ref_path: &str,
    symbol: &str,
    rust_paths: &mut super::removed_rust_paths::RustPathResolver,
) -> RefAttributionFacts {
    let Ok(source) = parser::read_file(utf8) else {
        return RefAttributionFacts::opaque();
    };
    let Ok(tree) = parser::parse_source(&source, LangId::Rust) else {
        return RefAttributionFacts::opaque();
    };
    let (independent_positions, local_positions) =
        super::removed_scope::rust_scope_positions(tree.root_node(), &source, symbol);
    // ファイル直下に同名の自由関数があるときだけ、修飾の無い出現がそこへ解決する。
    let rust_file_scope_positions = if independent_positions.is_empty() {
        HashSet::new()
    } else {
        super::removed_scope::rust_file_scope_positions(tree.root_node(), &source, symbol)
    };
    RefAttributionFacts {
        independent_positions,
        local_positions,
        rust_file_scope_positions,
        rust_sites: super::rust_ref_sites::member_positions(tree.root_node(), &source, symbol),
        rust_paths: rust_paths.positions(tree.root_node(), &source, symbol, ref_path),
        ..RefAttributionFacts::opaque()
    }
}

/// `import pkg.core as c` / `from pkg import core as c` の別名 → 実モジュール名 (末尾segment)。
/// 別名を持たない import は識別子がそのままモジュール名になるため収集不要。
fn collect_python_import_aliases(
    node: tree_sitter::Node,
    source: &[u8],
) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    // ファイル全体を辿るので、再帰せず木の深さに依存しない前順の走査で辿る
    // (同じ別名が複数あれば後の import が勝つ順序も再帰版と同じ)。
    let _: std::ops::ControlFlow<()> = crate::engine::tree_walk::walk_preorder(
        node,
        crate::engine::tree_walk::Children::Named,
        |node, _| {
            if node.kind() == "aliased_import"
                && let Some(name) = node.child_by_field_name("name")
                && let Some(alias) = node.child_by_field_name("alias")
                && let Ok(alias_text) = alias.utf8_text(source)
                && let Ok(name_text) = name.utf8_text(source)
            {
                let module = name_text.rsplit('.').next().unwrap_or(name_text);
                out.insert(alias_text.to_string(), module.to_string());
            }
            std::ops::ControlFlow::Continue(crate::engine::tree_walk::Walk::Descend)
        },
    );
    out
}

/// symbol の出現を走査し、すべてが属性アクセスなら true を返してレシーバの実効モジュール名を
/// `out` に積む。bare identifier としての出現を 1 件でも見つけたら false (証明不能)。
///
/// レシーバの名前でモジュールを特定できるのは、レシーバが識別子 (またはその属性連鎖) で、
/// 先頭の識別子がファイル内で import 文だけに束縛されている場合に限る。Python のモジュールは
/// 普通の値なので、変数・辞書・引数・呼び出し結果に入れて `backend.search(q)` /
/// `BACKENDS[name].search(q)` と呼べる。その場合は名前から削除モジュールかどうかを決められない
/// ため、証明しない (false)。`import_only` は先頭の識別子ごとの判定結果のキャッシュ。
fn collect_python_attribute_receivers(
    node: tree_sitter::Node,
    source: &[u8],
    symbol: &str,
    aliases: &std::collections::HashMap<String, String>,
    import_only: &mut HashMap<String, bool>,
    out: &mut HashSet<String>,
) -> bool {
    use crate::engine::tree_walk::{Children, Walk, walk_preorder};
    use std::ops::ControlFlow;

    // ファイル全体を辿るので、再帰せず木の深さに依存しない前順の走査で辿る。
    // 証明できない出現を見つけた時点で打ち切る (再帰版の `all` と同じ)。
    let walk = walk_preorder(node, Children::Named, |node, _| {
        if matches!(node.kind(), "string" | "comment") {
            return ControlFlow::Continue(Walk::Skip);
        }
        if node.kind() != "identifier" || node.utf8_text(source).ok() != Some(symbol) {
            return ControlFlow::Continue(Walk::Descend);
        }
        let Some(parent) = node.parent() else {
            return ControlFlow::Break(());
        };
        // `OBJ.symbol` の attribute 位置のみ属性アクセスとして扱う。
        // `symbol.attr` の object 位置は bare identifier 参照。
        if parent.kind() != "attribute"
            || parent.child_by_field_name("attribute").map(|n| n.id()) != Some(node.id())
        {
            return ControlFlow::Break(());
        }
        let Some(object) = parent.child_by_field_name("object") else {
            return ControlFlow::Break(());
        };
        // 添字・呼び出し結果など識別子の連鎖でないレシーバは、モジュールを入れた値かもしれない。
        let Some(base) = python_receiver_base_identifier(object) else {
            return ControlFlow::Break(());
        };
        let Ok(base_name) = base.utf8_text(source) else {
            return ControlFlow::Break(());
        };
        let root = {
            let mut root = node;
            while let Some(parent) = root.parent() {
                root = parent;
            }
            root
        };
        let bound_only_by_import = *import_only
            .entry(base_name.to_string())
            .or_insert_with(|| python_name_bound_only_by_import(root, source, base_name));
        if !bound_only_by_import {
            return ControlFlow::Break(());
        }
        let Some(name) = python_receiver_effective_name(object, source, aliases) else {
            return ControlFlow::Break(());
        };
        out.insert(name);
        ControlFlow::Continue(Walk::Skip)
    });
    walk.is_continue()
}

/// レシーバの識別子の連鎖 (`re` / `pkg.core`) の先頭の識別子。連鎖でなければ None。
fn python_receiver_base_identifier(mut object: tree_sitter::Node) -> Option<tree_sitter::Node> {
    // 属性の連鎖は入力次第で長くなるため、再帰せずループで受け手側へ辿る。
    loop {
        match object.kind() {
            "identifier" => return Some(object),
            "attribute" => {
                object
                    .child_by_field_name("attribute")
                    .filter(|member| member.kind() == "identifier")?;
                object = object.child_by_field_name("object")?;
            }
            _ => return None,
        }
    }
}

/// `name` がファイル内で import 文だけに束縛され、それ以外の出現がすべて読み取りと確定できる
/// 位置にあるか。代入・引数・ループ変数・辞書の値など、読み取りと確定できない出現が 1 件でも
/// あれば false (値が別のものに差し替わる・モジュールが別の名前へ渡る可能性を否定できない)。
/// `from x import *` はどの名前でも束縛し得るので false。
fn python_name_bound_only_by_import(root: tree_sitter::Node, source: &[u8], name: &str) -> bool {
    let mut imported = false;
    let mut nodes = vec![root];
    while let Some(node) = nodes.pop() {
        match node.kind() {
            "import_statement" | "import_from_statement" => {
                let mut cursor = node.walk();
                if node
                    .named_children(&mut cursor)
                    .any(|child| child.kind() == "wildcard_import")
                {
                    return false;
                }
                if python_import_binds_name(node, source, name) {
                    imported = true;
                }
                // import 文の中の名前は束縛の宣言で、読み取りではない。
                continue;
            }
            "identifier" if node.utf8_text(source).ok() == Some(name) => {
                // `obj.name` の属性名と `f(name=...)` のキーワード名は、この名前の変数の出現ではない。
                let field = node.parent().and_then(|parent| match parent.kind() {
                    "attribute" => parent.child_by_field_name("attribute"),
                    "keyword_argument" => parent.child_by_field_name("name"),
                    _ => None,
                });
                let not_variable = field.is_some_and(|field| field.id() == node.id());
                if !not_variable && !super::removed_scope::python_is_load(node) {
                    return false;
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        nodes.extend(node.named_children(&mut cursor));
    }
    imported
}

/// import 文が `name` をローカル束縛として導入するか (`import x` / `import x.y` は先頭の `x`、
/// `as` 別名は別名、`from p import x` は `x`)。
fn python_import_binds_name(statement: tree_sitter::Node, source: &[u8], name: &str) -> bool {
    let module = statement
        .child_by_field_name("module_name")
        .map(|node| node.id());
    let mut cursor = statement.walk();
    statement
        .children_by_field_name("name", &mut cursor)
        .filter(|node| Some(node.id()) != module)
        .any(|node| match node.kind() {
            "aliased_import" => node
                .child_by_field_name("alias")
                .is_some_and(|alias| alias.utf8_text(source).ok() == Some(name)),
            "dotted_name" => node
                .named_child(0)
                .is_some_and(|first| first.utf8_text(source).ok() == Some(name)),
            _ => false,
        })
}

/// レシーバ式の実効モジュール名。`re` → `re`、`pkg.core` → `core`、`c` (別名) → `core`。
/// 添字・関数呼び出しなど識別子で終わらない式は None。
fn python_receiver_effective_name(
    object: tree_sitter::Node,
    source: &[u8],
    aliases: &std::collections::HashMap<String, String>,
) -> Option<String> {
    let last = match object.kind() {
        "identifier" => object,
        "attribute" => object.child_by_field_name("attribute")?,
        _ => return None,
    };
    let text = last.utf8_text(source).ok()?;
    Some(
        aliases
            .get(text)
            .cloned()
            .unwrap_or_else(|| text.to_string()),
    )
}

#[allow(clippy::too_many_arguments)]
fn analyze_js_ts_facts(
    utf8: &camino::Utf8Path,
    lang: LangId,
    ref_path: &str,
    symbol: &str,
    external_pkgs: &HashSet<String>,
    member_proof: bool,
    dir: &str,
    export_cache: &mut super::js_ref_sites::JsExportOriginCache,
) -> RefAttributionFacts {
    let opaque = RefAttributionFacts {
        is_js_ts: true,
        ..RefAttributionFacts::opaque()
    };
    // Angular テンプレート (`.html`) は TS として解析できない。位置を分類せず全て未証明にする。
    if !matches!(
        LangId::from_path(utf8),
        Ok(LangId::Javascript | LangId::Typescript | LangId::Tsx)
    ) {
        return opaque;
    }
    let Ok(source) = parser::read_file(utf8) else {
        return opaque;
    };
    let Ok(tree) = parser::parse_source(&source, lang) else {
        return opaque;
    };
    let root = tree.root_node();
    let mut facts = opaque;
    let dependency_scan = super::js_member_origin::dependency_targets(ref_path, &source, root);
    if member_proof && !dependency_scan.opaque {
        facts.js_member_evidence = super::js_member_origin::collect_member_evidence(
            dir,
            ref_path,
            &source,
            root,
            symbol,
            &dependency_scan.targets,
        );
    }
    facts.js_dependency_opaque = dependency_scan.opaque;
    facts.js_dependency_targets = Some(Rc::new(dependency_scan.targets));
    facts.js_sites = super::js_ref_sites::classify_js_ref_sites(
        dir,
        ref_path,
        &source,
        root,
        symbol,
        external_pkgs,
        export_cache,
    );
    facts
}

fn analyze_bash_facts(utf8: &camino::Utf8Path, ref_path: &str) -> RefAttributionFacts {
    let Ok(source) = parser::read_file(utf8) else {
        return RefAttributionFacts::opaque();
    };
    let Ok(tree) = parser::parse_source(&source, LangId::Bash) else {
        return RefAttributionFacts::opaque();
    };
    let mut sourced = HashSet::new();
    collect_bash_literal_sources(tree.root_node(), &source, ref_path, &mut sourced);
    RefAttributionFacts {
        is_bash: true,
        bash_sourced_candidates: Some(Rc::new(sourced)),
        ..RefAttributionFacts::opaque()
    }
}

/// bash AST から `source <path>` / `. <path>` のリテラル引数を集め、repo 相対の解決候補に
/// 展開する。変数・コマンド置換を含む引数は「削除ファイルを指す証拠」にならないため
/// 候補に入れない (このガードは SelfDefined 証明を blocking に戻す用途のみで、候補が
/// 増える方向は false negative を生まない)。
fn collect_bash_literal_sources(
    node: tree_sitter::Node,
    source: &[u8],
    ref_path: &str,
    out: &mut HashSet<String>,
) {
    // ファイル全体を辿るので、再帰せず木の深さに依存しない前順の走査で辿る。
    let _: std::ops::ControlFlow<()> = crate::engine::tree_walk::walk_preorder(
        node,
        crate::engine::tree_walk::Children::All,
        |node, _| {
            if node.kind() == "command" {
                let mut cursor = node.walk();
                let children: Vec<_> = node.named_children(&mut cursor).collect();
                if let Some(name) = children.first().filter(|c| c.kind() == "command_name")
                    && matches!(name.utf8_text(source).ok(), Some("source") | Some("."))
                    && let Some(arg) = children.get(1)
                    && let Some(literal) = bash_literal_word(*arg, source)
                {
                    add_bash_source_candidates(ref_path, &literal, out);
                }
            }
            std::ops::ControlFlow::Continue(crate::engine::tree_walk::Walk::Descend)
        },
    );
}

/// bash の引数ノードが expansion を含まないリテラルであればテキストを返す。
fn bash_literal_word(node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    match node.kind() {
        "word" => node.utf8_text(source).ok().map(str::to_string),
        "string" => {
            // `"lib.sh"`: named child が string_content のみならリテラル
            let mut cursor = node.walk();
            let children: Vec<_> = node.named_children(&mut cursor).collect();
            if children.len() == 1 && children[0].kind() == "string_content" {
                children[0].utf8_text(source).ok().map(str::to_string)
            } else {
                None
            }
        }
        "raw_string" => node
            .utf8_text(source)
            .ok()
            .map(|t| t.trim_matches('\'').to_string()),
        _ => None,
    }
}

/// source 引数の解決候補。bash の source は実行時 cwd 依存のため、参照ファイルの親
/// ディレクトリ相対と repo ルート相対の両方を候補にする (候補が増える方向は blocking
/// 維持側にしか働かない)。絶対パスは repo 相対比較できないため対象外。
fn add_bash_source_candidates(ref_path: &str, arg: &str, out: &mut HashSet<String>) {
    if arg.starts_with('/') || arg.is_empty() {
        return;
    }
    let rel_spec;
    let spec = if arg.starts_with("./") || arg.starts_with("../") {
        arg
    } else {
        rel_spec = format!("./{arg}");
        &rel_spec
    };
    if let Some(p) = normalize_relative_specifier(ref_path, spec) {
        out.insert(p);
    }
    // repo ルート相対 (cwd = repo root で実行される慣習)
    if let Some(p) = normalize_repo_root_relative(arg) {
        out.insert(p);
    }
}

/// repo 相対パス `base_file` の親ディレクトリを基準に、相対 specifier `spec`
/// (`./x` / `../x`) を repo 相対へ正規化する。`..` がリポジトリルートを突き抜ける場合や
/// 相対 specifier でない場合は None。
pub(crate) fn normalize_relative_specifier(base_file: &str, spec: &str) -> Option<String> {
    if !(spec.starts_with("./") || spec.starts_with("../")) {
        return None;
    }
    let mut components: Vec<&str> = base_file.split('/').collect();
    components.pop(); // ファイル名を除き親 dir に
    for seg in spec.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                components.pop()?;
            }
            other => components.push(other),
        }
    }
    if components.is_empty() {
        return None;
    }
    Some(components.join("/"))
}

/// `arg` (相対形式) を repo ルート基準で正規化する。`..` 成分がルートを突き抜ける場合は None。
fn normalize_repo_root_relative(arg: &str) -> Option<String> {
    let mut components: Vec<&str> = Vec::new();
    for seg in arg.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                components.pop()?;
            }
            other => components.push(other),
        }
    }
    if components.is_empty() {
        return None;
    }
    Some(components.join("/"))
}

/// TS/JS の相対 import specifier を実体ファイルの解決候補 (repo 相対パス集合) に展開する。
/// - `.js` / `.mjs` / `.cjs` 指定は TS 実体 (`.ts` / `.tsx` / `.mts` / `.cts`) も候補に含める
///   (moduleResolution=node16/bundler の ESM 慣習)
/// - JS 系拡張子なしは各拡張子の付与と `index.*` を展開する
///
/// 相対でない / ルート突き抜けは None (解決不能)。
pub(crate) fn relative_import_candidates(base_file: &str, spec: &str) -> Option<HashSet<String>> {
    const EXTS: [&str; 6] = ["ts", "tsx", "js", "jsx", "mjs", "cjs"];
    let normalized = normalize_relative_specifier(base_file, spec)?;
    let mut out = HashSet::new();
    let known_ext = std::path::Path::new(&normalized)
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| EXTS.contains(e) || matches!(*e, "mts" | "cts"))
        .map(str::to_string);
    match known_ext.as_deref() {
        Some("js") | Some("jsx") => {
            let stem = normalized.rsplit_once('.').map(|(s, _)| s).unwrap_or("");
            out.insert(format!("{stem}.ts"));
            out.insert(format!("{stem}.tsx"));
            out.insert(normalized);
        }
        Some("mjs") => {
            let stem = normalized.rsplit_once('.').map(|(s, _)| s).unwrap_or("");
            out.insert(format!("{stem}.mts"));
            out.insert(normalized);
        }
        Some("cjs") => {
            let stem = normalized.rsplit_once('.').map(|(s, _)| s).unwrap_or("");
            out.insert(format!("{stem}.cts"));
            out.insert(normalized);
        }
        Some(_) => {
            out.insert(normalized);
        }
        None => {
            for e in EXTS {
                out.insert(format!("{normalized}.{e}"));
            }
            for e in EXTS {
                out.insert(format!("{normalized}/index.{e}"));
            }
            out.insert(normalized);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_member_proof_requires_matching_normalized_deleted_paths() {
        let evidence = Rc::new(super::super::js_member_origin::JsMemberEvidence {
            class_files: Rc::new(["src/util.ts".to_string(), "src/util.js".to_string()].into()),
            dependency_targets: Rc::new(HashSet::new()),
        });
        let attributed = RefAttribution {
            ref_lang: Some(LangId::Typescript),
            ref_path: String::new(),
            rust_binds_in_file: false,
            origin: RefOrigin::JsMemberOfResolvedClass(evidence),
        };
        for old_path in [
            "src/util.js",
            "src/Util.js",
            "./src/util.js",
            "/src/util.js",
        ] {
            assert!(!proves_survivor_origin(
                &attributed,
                &RemovedCandidateRef {
                    old_path,
                    name: "Formatter.formatItem",
                    kind: "method",
                    rust_module_function: None,
                },
            ));
        }
        assert!(proves_survivor_origin(
            &attributed,
            &RemovedCandidateRef {
                old_path: "dist/util.js",
                name: "Formatter.formatItem",
                kind: "method",
                rust_module_function: None,
            },
        ));
    }

    #[test]
    fn normalize_relative_specifier_resolves_parent_dirs() {
        assert_eq!(
            normalize_relative_specifier("api/test/config.test.ts", "../src/config").as_deref(),
            Some("api/src/config")
        );
        assert_eq!(
            normalize_relative_specifier("src/index.ts", "./services/gws").as_deref(),
            Some("src/services/gws")
        );
        // ルート突き抜けは None
        assert_eq!(normalize_relative_specifier("a.ts", "../../x"), None);
        // 相対でない specifier は None
        assert_eq!(normalize_relative_specifier("a.ts", "pkg"), None);
        assert_eq!(normalize_relative_specifier("a.ts", "@/lib"), None);
    }

    #[test]
    fn relative_import_candidates_expands_extensions_and_index() {
        let c = relative_import_candidates("api/test/config.test.ts", "../src/config")
            .expect("resolved");
        assert!(c.contains("api/src/config.ts"));
        assert!(c.contains("api/src/config.mjs"));
        assert!(c.contains("api/src/config/index.ts"));
        // `.js` 指定は TS 実体も候補に
        let c = relative_import_candidates("src/a.ts", "./b.js").expect("resolved");
        assert!(c.contains("src/b.js"));
        assert!(c.contains("src/b.ts"));
        assert!(c.contains("src/b.tsx"));
        // 拡張子付きピリオド入りファイル名 (`.util` は JS 拡張子でない) は拡張子なし扱い
        let c = relative_import_candidates("src/a.ts", "./b.util").expect("resolved");
        assert!(c.contains("src/b.util.ts"));
    }

    /// 言語による証明を挟まない (= 従来ロジックのみを見る) ための helper。
    /// `ref_lang` を None にすると `proves_survivor_origin` の言語判定を必ず素通りする。
    fn attr(origin: RefOrigin) -> RefAttribution {
        RefAttribution {
            ref_lang: None,
            ref_path: String::new(),
            rust_binds_in_file: false,
            origin,
        }
    }

    fn candidate<'a>(old_path: &'a str, name: &'a str, kind: &'a str) -> RemovedCandidateRef<'a> {
        RemovedCandidateRef {
            old_path,
            name,
            kind,
            rust_module_function: None,
        }
    }

    #[test]
    fn proves_survivor_origin_matrix() {
        let deleted = candidate("plugins/setup.mjs", "helper", "function");
        // 解決先の全候補が公開名を由来の分かる形で export している import → 証明
        let a = attr(RefOrigin::ImportResolved {
            candidates: Rc::new(
                [
                    "api/src/config.ts".to_string(),
                    "api/src/config.js".to_string(),
                ]
                .into(),
            ),
            export_proven: true,
        });
        assert!(proves_survivor_origin(&a, &deleted));
        // 候補に old_path が含まれる → 証明失敗 (削除ファイルへの残存参照)
        let a = attr(RefOrigin::ImportResolved {
            candidates: Rc::new(["plugins/setup.mjs".to_string()].into()),
            export_proven: true,
        });
        assert!(!proves_survivor_origin(&a, &deleted));
        // 候補の export の由来を確かめられない (`export *` だけの barrel 等) → 証明失敗
        let a = attr(RefOrigin::ImportResolved {
            candidates: Rc::new(["lib/barrel.ts".to_string()].into()),
            export_proven: false,
        });
        assert!(!proves_survivor_origin(&a, &deleted));
        // 同ファイルの通常の宣言への束縛 (TS/JS) → 証明。依存先に削除ファイルがある・
        // 依存先を静的に決められない場合は証明しない
        let local = |targets: &[&str], opaque: bool| {
            attr(RefOrigin::JsLocalDeclaration {
                dependency_targets: Rc::new(targets.iter().map(|t| t.to_string()).collect()),
                dependency_opaque: opaque,
            })
        };
        assert!(proves_survivor_origin(
            &local(&["lib/other.ts"], false),
            &deleted
        ));
        assert!(!proves_survivor_origin(
            &local(&["plugins/setup.mjs"], false),
            &deleted
        ));
        assert!(!proves_survivor_origin(&local(&[], true), &deleted));
        // 削除シンボルを指し得ない位置 → 証明
        assert!(proves_survivor_origin(&attr(RefOrigin::NonUse), &deleted));
        // bash の同ファイル定義 → 証明
        let a = attr(RefOrigin::SelfDefined {
            sourced_candidates: None,
        });
        assert!(proves_survivor_origin(&a, &deleted));
        // bash: リテラル source が old_path を指す → 証明失敗
        let a = attr(RefOrigin::SelfDefined {
            sourced_candidates: Some(Rc::new(["scripts/deleted.sh".to_string()].into())),
        });
        assert!(!proves_survivor_origin(
            &a,
            &candidate("scripts/deleted.sh", "helper", "function")
        ));
        assert!(proves_survivor_origin(
            &a,
            &candidate("scripts/other.sh", "helper", "function")
        ));
        // 証明不能は常に false
        assert!(!proves_survivor_origin(
            &attr(RefOrigin::Unproven),
            &candidate("x.ts", "helper", "function")
        ));
    }

    /// 言語が非互換なら origin が Unproven でも証明が成立する。互換なら従来どおり従う。
    #[test]
    fn proves_survivor_origin_uses_cross_language_incompatibility() {
        let deleted = candidate("scripts/core.py", "search", "function");
        for lang in [
            LangId::Php,
            LangId::C,
            LangId::Javascript,
            LangId::Typescript,
            LangId::Tsx,
            LangId::Rust,
            LangId::Java,
        ] {
            let a = RefAttribution {
                ref_lang: Some(lang),
                ref_path: String::new(),
                rust_binds_in_file: false,
                origin: RefOrigin::Unproven,
            };
            assert!(
                proves_survivor_origin(&a, &deleted),
                "{lang:?} は Python 定義へ識別子束縛できないので証明成立すべき"
            );
        }
        // 同一言語 (Python) の bare 参照は従来どおり証明不能 = blocking 維持
        let same_lang = RefAttribution {
            ref_lang: Some(LangId::Python),
            ref_path: String::new(),
            rust_binds_in_file: false,
            origin: RefOrigin::Unproven,
        };
        assert!(!proves_survivor_origin(&same_lang, &deleted));
        // 言語が判らない参照 (Android XML 等) も fail-closed
        let unknown = RefAttribution {
            ref_lang: None,
            ref_path: String::new(),
            rust_binds_in_file: false,
            origin: RefOrigin::Unproven,
        };
        assert!(!proves_survivor_origin(&unknown, &deleted));
        // 削除側が C ABI を公開する言語なら、ctypes/cinterop 経由で束縛され得るため証明しない
        let c_deleted = candidate("native/lib.c", "search", "function");
        for lang in [LangId::Python, LangId::Kotlin, LangId::Php, LangId::Ruby] {
            let a = RefAttribution {
                ref_lang: Some(lang),
                ref_path: String::new(),
                rust_binds_in_file: false,
                origin: RefOrigin::Unproven,
            };
            assert!(
                !proves_survivor_origin(&a, &c_deleted),
                "{lang:?} は C ABI シンボルを識別子として取り込み得るので blocking 維持すべき"
            );
        }
    }

    /// Python の属性アクセスのみの参照は、レシーバが削除モジュールでなければ証明成立。
    /// メソッド削除では属性アクセスが正当な参照形なので証明しない。
    #[test]
    fn proves_survivor_origin_python_attribute_access() {
        let receivers = |names: &[&str]| {
            attr(RefOrigin::PythonAttributeAccess {
                receiver_module_names: Rc::new(
                    names
                        .iter()
                        .map(|s| (*s).to_string())
                        .collect::<HashSet<_>>(),
                ),
            })
        };
        // `re.search(...)` のみ → 削除した core.py の search とは無関係
        assert!(proves_survivor_origin(
            &receivers(&["re", "self"]),
            &candidate("scripts/core.py", "search", "function")
        ));
        // `core.search(...)` → 削除モジュールへの残存参照
        assert!(!proves_survivor_origin(
            &receivers(&["re", "core"]),
            &candidate("scripts/core.py", "search", "function")
        ));
        // `pkg/__init__.py` の削除はパッケージ名 `pkg` で参照される
        assert!(!proves_survivor_origin(
            &receivers(&["pkg"]),
            &candidate("pkg/__init__.py", "search", "function")
        ));
        // `pkg/__init__.py` の `from .util import helper` 経由で `pkg.helper()` と呼べるため、
        // 祖先パッケージ名がレシーバなら証明しない (直下・多段とも)
        assert!(!proves_survivor_origin(
            &receivers(&["pkg"]),
            &candidate("pkg/util.py", "helper", "function")
        ));
        assert!(!proves_survivor_origin(
            &receivers(&["pkg"]),
            &candidate("pkg/sub/util.py", "helper", "function")
        ));
        assert!(
            !proves_survivor_origin(
                &receivers(&["sub"]),
                &candidate("pkg/sub/__init__.py", "helper", "function")
            ) && !proves_survivor_origin(
                &receivers(&["pkg"]),
                &candidate("pkg/sub/__init__.py", "helper", "function")
            )
        );
        // 対照: 削除モジュールとも祖先パッケージとも無関係なレシーバは従来どおり証明する
        assert!(proves_survivor_origin(
            &receivers(&["re", "other"]),
            &candidate("pkg/util.py", "helper", "function")
        ));
        // メソッド削除は属性アクセスこそが正当な参照形 → 証明しない (fail-closed)
        assert!(!proves_survivor_origin(
            &receivers(&["re"]),
            &candidate("scripts/core.py", "BM25.fit", "method")
        ));
        assert!(!proves_survivor_origin(
            &receivers(&["re"]),
            &candidate("scripts/core.py", "fit", "method")
        ));
    }

    #[test]
    fn python_module_names_include_package_name_and_ancestors() {
        assert_eq!(python_module_names("scripts/core.py"), ["core", "scripts"]);
        assert_eq!(python_module_names("pkg/__init__.py"), ["pkg"]);
        assert_eq!(python_module_names("core.py"), ["core"]);
        assert_eq!(
            python_module_names("src/pkg/sub/util.py"),
            ["util", "sub", "pkg", "src"]
        );
        assert_eq!(python_module_names("a/pkg/__init__.py"), ["pkg", "a"]);
    }

    fn python_facts_for(src: &str, symbol: &str) -> RefAttributionFacts {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ref.py");
        std::fs::write(&path, src).expect("write");
        let utf8 = camino::Utf8Path::from_path(&path).expect("utf8");
        analyze_python_facts(utf8, symbol)
    }

    /// 属性アクセスだけのファイルはレシーバ名を集め、bare 出現があれば証明を諦める。
    #[test]
    fn analyze_python_facts_detects_attribute_only_usage() {
        let facts = python_facts_for(
            "import re\n\ndef check(pw):\n    return re.search('x', pw)\n",
            "search",
        );
        let receivers = facts.python_attribute_receivers.expect("attribute only");
        assert!(receivers.contains("re"), "receivers={receivers:?}");

        // 別名 import はモジュール実名へ解決する
        let facts = python_facts_for(
            "import scripts.core as c\n\ndef run():\n    return c.search(1)\n",
            "search",
        );
        let receivers = facts.python_attribute_receivers.expect("attribute only");
        assert!(receivers.contains("core"), "receivers={receivers:?}");

        // ドット連鎖は直前の segment を実効モジュール名にする
        let facts = python_facts_for(
            "import scripts\n\ndef run():\n    return scripts.core.search(1)\n",
            "search",
        );
        let receivers = facts.python_attribute_receivers.expect("attribute only");
        assert!(receivers.contains("core"), "receivers={receivers:?}");

        // bare 呼び出しが混ざれば証明不能
        let facts = python_facts_for(
            "import re\n\ndef run(pw):\n    if re.search('x', pw):\n        return search(pw)\n    return None\n",
            "search",
        );
        assert!(facts.python_attribute_receivers.is_none());

        // from import の識別子出現も bare なので証明不能
        let facts = python_facts_for("from core import search\n", "search");
        assert!(facts.python_attribute_receivers.is_none());
    }

    /// モジュールは普通の値なので、辞書・変数・引数・呼び出し結果に入れたレシーバ
    /// (`BACKENDS[name].search(q)` / `backend.search(q)`) の名前からは削除モジュールかどうかを
    /// 決められない。レシーバの先頭が import 文だけに束縛された名前のときだけ証明に使う。
    #[test]
    fn analyze_python_facts_rejects_receivers_not_bound_only_by_import() {
        for source in [
            // 辞書に入れたモジュールの添字アクセス
            "from pkg import core, fast\nBACKENDS = {\"core\": core, \"fast\": fast}\ndef run(name, q):\n    return BACKENDS[name].search(q)\n",
            // 変数に入れたモジュール
            "from pkg import core\nbackend = core\ndef run(q):\n    return backend.search(q)\n",
            // 引数・メソッドの self
            "def run(mod, q):\n    return mod.search(q)\n",
            "class Finder:\n    def run(self, q):\n        return self.search(q)\n",
            // 呼び出し結果
            "def run(q):\n    return get_backend().search(q)\n",
            // import した名前の再束縛・ワイルドカード import
            "import re\nre = other\ndef run(x):\n    return re.search('x', x)\n",
            "from tools import *\nimport re\ndef run(x):\n    return re.search('x', x)\n",
            // import した名前を辞書の値として渡すと、別の名前から呼ばれ得る
            "import re\nTABLE = {'r': re}\ndef run(x):\n    return re.search('x', x)\n",
        ] {
            let facts = python_facts_for(source, "search");
            assert!(
                facts.python_attribute_receivers.is_none(),
                "証明してはならない: {source}"
            );
        }
        // 対照: import 文だけに束縛された名前は従来どおりレシーバ名を集める
        // (属性名・キーワード名としての同名の出現は変数の出現に数えない)
        for (source, receiver) in [
            (
                "import re\ndef run(x):\n    return re.search('x', x)\n",
                "re",
            ),
            (
                "import re\nobj.re = 1\nf(re=2)\ndef run(x):\n    return re.search('x', x)\n",
                "re",
            ),
            (
                "from pkg import core\ndef run(q):\n    return core.search(q)\n",
                "core",
            ),
            (
                "import pkg.core\ndef run(q):\n    return pkg.core.search(q)\n",
                "core",
            ),
        ] {
            let facts = python_facts_for(source, "search");
            let receivers = facts
                .python_attribute_receivers
                .unwrap_or_else(|| panic!("証明に使えるはず: {source}"));
            assert!(receivers.contains(receiver), "{source}: {receivers:?}");
        }
    }

    /// 修正前の再帰と同じ形の参照実装 (浅い入力でだけ使う)。
    fn collect_python_import_aliases_recursive(
        node: tree_sitter::Node,
        source: &[u8],
    ) -> std::collections::HashMap<String, String> {
        let mut out = std::collections::HashMap::new();
        fn walk(
            node: tree_sitter::Node,
            source: &[u8],
            out: &mut std::collections::HashMap<String, String>,
        ) {
            if node.kind() == "aliased_import"
                && let Some(name) = node.child_by_field_name("name")
                && let Some(alias) = node.child_by_field_name("alias")
                && let Ok(alias_text) = alias.utf8_text(source)
                && let Ok(name_text) = name.utf8_text(source)
            {
                let module = name_text.rsplit('.').next().unwrap_or(name_text);
                out.insert(alias_text.to_string(), module.to_string());
            }
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                walk(child, source, out);
            }
        }
        walk(node, source, &mut out);
        out
    }

    /// 修正前の再帰と同じ形の参照実装 (浅い入力でだけ使う)。
    fn python_receiver_base_identifier_recursive(
        object: tree_sitter::Node,
    ) -> Option<tree_sitter::Node> {
        match object.kind() {
            "identifier" => Some(object),
            "attribute" => {
                object
                    .child_by_field_name("attribute")
                    .filter(|member| member.kind() == "identifier")?;
                python_receiver_base_identifier_recursive(object.child_by_field_name("object")?)
            }
            _ => None,
        }
    }

    /// 修正前の再帰と同じ形の参照実装 (浅い入力でだけ使う)。
    fn collect_python_attribute_receivers_recursive(
        node: tree_sitter::Node,
        source: &[u8],
        symbol: &str,
        aliases: &std::collections::HashMap<String, String>,
        import_only: &mut HashMap<String, bool>,
        out: &mut HashSet<String>,
    ) -> bool {
        if matches!(node.kind(), "string" | "comment") {
            return true;
        }
        if node.kind() == "identifier" && node.utf8_text(source).ok() == Some(symbol) {
            let Some(parent) = node.parent() else {
                return false;
            };
            if parent.kind() != "attribute"
                || parent.child_by_field_name("attribute").map(|n| n.id()) != Some(node.id())
            {
                return false;
            }
            let Some(object) = parent.child_by_field_name("object") else {
                return false;
            };
            let Some(base) = python_receiver_base_identifier_recursive(object) else {
                return false;
            };
            let Ok(base_name) = base.utf8_text(source) else {
                return false;
            };
            let root = {
                let mut root = node;
                while let Some(parent) = root.parent() {
                    root = parent;
                }
                root
            };
            let bound_only_by_import = *import_only
                .entry(base_name.to_string())
                .or_insert_with(|| python_name_bound_only_by_import(root, source, base_name));
            if !bound_only_by_import {
                return false;
            }
            let Some(name) = python_receiver_effective_name(object, source, aliases) else {
                return false;
            };
            out.insert(name);
            return true;
        }
        let mut cursor = node.walk();
        node.named_children(&mut cursor).all(|child| {
            collect_python_attribute_receivers_recursive(
                child,
                source,
                symbol,
                aliases,
                import_only,
                out,
            )
        })
    }

    /// 反復版の Python の別名・レシーバの収集が、浅い入力で再帰版と同じ結果 (戻り値・
    /// 集めたレシーバ・import だけの束縛の判定キャッシュ) を返す。
    #[test]
    fn python_attribute_helpers_match_recursive_version() {
        let sources = [
            "import pkg.core as c\nimport re\nfrom pkg import core as c2\nimport other as c\n\
             re.search(x)\nc.search(y)\npkg.core.search(z)\nc2.search(1)\ns = 'search' # search\n",
            "from m import search\nsearch()\n",
            "import re\nx = re\nre.search(1)\n",
            "import re\nBACKENDS['a'].search(q)\n",
            "import re\nf().search(1)\nre.search.attr\n",
            "def f():\n    import re as r\n    return r.search(1)\nclass K:\n    def g(self):\n        return self.search\n",
            "import a.b.c as d\nd.search(1)\na.b.c.search(2)\n(a).search(3)\n",
        ];
        let mut accepted = 0;
        for src in sources {
            let tree = parser::parse_source(src.as_bytes(), LangId::Python).unwrap();
            let root = tree.root_node();
            let aliases = collect_python_import_aliases(root, src.as_bytes());
            assert_eq!(
                aliases,
                collect_python_import_aliases_recursive(root, src.as_bytes()),
                "{src}"
            );
            for symbol in ["search", "attr", "re", "core", "missing"] {
                let mut import_only = HashMap::new();
                let mut out = HashSet::new();
                let mut import_only_recursive = HashMap::new();
                let mut out_recursive = HashSet::new();
                let result = collect_python_attribute_receivers(
                    root,
                    src.as_bytes(),
                    symbol,
                    &aliases,
                    &mut import_only,
                    &mut out,
                );
                assert_eq!(
                    result,
                    collect_python_attribute_receivers_recursive(
                        root,
                        src.as_bytes(),
                        symbol,
                        &aliases,
                        &mut import_only_recursive,
                        &mut out_recursive,
                    ),
                    "{symbol}: {src}"
                );
                assert_eq!(out, out_recursive, "{symbol}: {src}");
                assert_eq!(import_only, import_only_recursive, "{symbol}: {src}");
                accepted += usize::from(result && !out.is_empty());
            }
            let _: std::ops::ControlFlow<()> = crate::engine::tree_walk::walk_preorder(
                root,
                crate::engine::tree_walk::Children::All,
                |node, _| {
                    assert_eq!(
                        python_receiver_base_identifier(node).map(|n| n.id()),
                        python_receiver_base_identifier_recursive(node).map(|n| n.id()),
                        "{}",
                        node.to_sexp()
                    );
                    std::ops::ControlFlow::Continue(crate::engine::tree_walk::Walk::Descend)
                },
            );
        }
        assert!(accepted >= 2, "属性アクセスだけと証明できる場合も比べる");
    }

    /// 修正前の再帰と同じ形の参照実装 (浅い入力でだけ使う)。
    fn collect_bash_literal_sources_recursive(
        node: tree_sitter::Node,
        source: &[u8],
        ref_path: &str,
        out: &mut HashSet<String>,
    ) {
        if node.kind() == "command" {
            let mut cursor = node.walk();
            let children: Vec<_> = node.named_children(&mut cursor).collect();
            if let Some(name) = children.first().filter(|c| c.kind() == "command_name")
                && matches!(name.utf8_text(source).ok(), Some("source") | Some("."))
                && let Some(arg) = children.get(1)
                && let Some(literal) = bash_literal_word(*arg, source)
            {
                add_bash_source_candidates(ref_path, &literal, out);
            }
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            collect_bash_literal_sources_recursive(child, source, ref_path, out);
        }
    }

    /// 反復版が、浅い入力で再帰版と同じ `source` の候補を集める。
    #[test]
    fn collect_bash_literal_sources_matches_recursive_version() {
        let src = "source ./lib.sh\n. \"lib2.sh\"\nsource 'lib3.sh'\n\
                   if true; then source lib4.sh; fi\nf() { . ../lib5.sh; }\n\
                   source $VAR/lib6.sh\necho source x\n( source sub/lib7.sh )\n";
        let tree = parser::parse_source(src.as_bytes(), LangId::Bash).unwrap();
        let mut iterative = HashSet::new();
        let mut recursive = HashSet::new();
        collect_bash_literal_sources(
            tree.root_node(),
            src.as_bytes(),
            "scripts/run.sh",
            &mut iterative,
        );
        collect_bash_literal_sources_recursive(
            tree.root_node(),
            src.as_bytes(),
            "scripts/run.sh",
            &mut recursive,
        );
        assert_eq!(iterative, recursive);
        assert!(iterative.len() >= 6, "{iterative:?}");
    }
}
