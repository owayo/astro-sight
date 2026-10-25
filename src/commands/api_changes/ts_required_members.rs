//! TypeScript の interface / object 型の type alias の必須追加・型縮小・プロパティ削除を、
//! 値を作る箇所と受け取る箇所の義務から証明する (Issue #89 / #138 / #139 / #141)。
//!
//! 名前で集めた型の参照を構文上の役割で分け、役割ごとの義務をすべて証明できたときだけ閉じる。
//! 参照行が変更行にあることは追随の証拠にしない (`label(row: Row)` の行を整形しただけで、
//! 呼び出し側の確認が飛ぶため)。
//!
//! - 値を作る箇所: 戻り値の型 (`function f(): Row`)・`const x: Row = ...`・`... satisfies Row`。
//!   作る値が「対象の型と証明できる式」であること
//! - 受け取る箇所: ファイル直下の関数宣言の必須引数 (`function f(row: Row)`)。本体でその引数へ
//!   書き込まず、全ての直接呼び出しが「対象の型と証明できる式」を渡していること。呼び出しは
//!   関数名で別途検索する
//! - 公開するだけの箇所: 別名なしの `export { Row }`。値の名前空間の出現 (同名の変数・JSX の
//!   要素名・プロパティ名) は interface / type alias を指さないので数えない
//!
//! 「対象の型と証明できる式」は次の 3 つ。
//! - 追加した全プロパティを 1 つずつ持ち、値の型が追加したメンバーの型に適合する object literal
//!   (spread・computed key があれば不成立。各プロパティの行が変更行)
//! - 戻り値の型がちょうど対象の型のファイル直下の関数宣言の呼び出し
//! - 型注釈がちょうど対象の型の必須引数・変数
//!
//! 値の型は単純な型 (string / number / boolean / bigint / null / undefined / literal /
//! これらの union と配列 / any / unknown) だけを扱う。型注釈のない変数は literal の widening が
//! あるので、const の primitive literal だけを literal 型として扱う。値の中の `satisfies` と、
//! primitive 以外への `as const` は literal の型付けを変えるので読まない。
//!
//! 保証するのは「名前で観測した参照がすべて追随済み」まで。型の名前が現れない推論経由の依存
//! (`const x: { note?: number } = toRow()` のような別の型への構造的な代入) は追えない。
//! `typeof` による型の取り出しは、作った値を束縛する名前 (関数・変数と、値を入れた外側の
//! オブジェクト・配列) の参照で確かめる。名前で利用者を追えない形 (default export・別名での
//! 公開や import・クラスのメンバー) と、埋め込みソース・JS 系ファイルのコメント (JSDoc) への
//! 出現は証明しない。

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ops::ControlFlow;
use std::path::Path;
use std::rc::Rc;

use rayon::prelude::*;
use tree_sitter::Node;

use crate::engine::js_scope::{Namespace, ScopeBinding, pattern_binds_name, resolve_binding_scope};
use crate::engine::tree_walk::{Children, Walk, any_node, walk_preorder};
use crate::language::LangId;
use crate::models::reference::{RefKind, SymbolReference};

use super::ref_index::{
    ApiClosureCaches, DiffFileLookup, ParsedRefFile, RefImportRole, changed_lines_for_ref_path,
    diff_path_matches_ref, embedded_js_sources_may_mention, parsed_ref_file_shared,
    ref_import_role, ref_is_pass_through_reexport, ref_name_node,
};

mod local_producers;
mod narrowed_uses;
mod removed_uses;
mod return_producers;

/// 配列型の入れ子 (`string[][]`) と式の配列 literal の入れ子をたどる上限。
const MAX_ARRAY_DEPTH: usize = 8;
/// 括弧・`satisfies`・`as const` を剥がす上限。超えたら証明しない。
const MAX_UNWRAP: usize = 64;
/// `typeof a.b.c` のメンバー連鎖をたどる上限。
const MAX_TYPE_QUERY_CHAIN: usize = 16;
/// tsconfig / jsconfig 1 ファイルの読み込み上限。超えたら証明しない。
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
/// 関数・変数の名前の参照を探すファイル。呼び出し側と `typeof` を型検査するのは tsc が
/// 読む拡張子のファイルだけ (`.vue` などの埋め込みソースは別に確かめる)。他言語のファイルを
/// 読まない分、大きなリポジトリでの検索が速くなる (約 45,000 ファイルで 31 秒 → 6 秒)。
const NAME_REFS_GLOB: &str = "**/*.{ts,tsx,mts,cts,js,jsx,mjs,cjs}";
/// JSDoc と文字列で書いた名前を調べるファイル 1 つの読み込み上限。超えたら名前が現れ得ると
/// みなす。
const MAX_SOURCE_SCAN_BYTES: u64 = 16 * 1024 * 1024;

/// 値の型・メンバーの型の要素。union は要素の集合 (整列・重複なし) で表す。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum TypeAtom {
    String,
    Number,
    Boolean,
    BigInt,
    Null,
    Undefined,
    /// 文字列 literal 型。`None` は値を静的に比べられない (escape を含む) literal。
    StrLit(Option<String>),
    /// 数値 literal 型 (表記のまま。`1` と `1.0` は別とみなす)。
    NumLit(String),
    /// bigint literal 型 (`1n`。表記のまま)。
    BigIntLit(String),
    True,
    False,
    /// 要素の型の union。空は要素の無い配列 literal (`[]`)。
    Array(Vec<TypeAtom>),
    Any,
    Unknown,
}

/// 必須プロパティ追加・既存プロパティの型縮小・純粋なプロパティ削除の分類。
pub(crate) enum RequiredMemberChange {
    /// この変更種別には当たらない (従来の判定へ)。
    NotApplicable,
    /// 証明に対応する形の変更。
    Supported(RequiredMemberDelta),
    /// 証明に対応しないメンバー変更。参照行が変更行にあるだけの証拠でも閉じない。
    Unsupported,
}

/// 値の構築で検査するプロパティと、利用監査のモード。
pub(crate) struct RequiredMemberDelta {
    /// 値の検査対象。追加・縮小では対象プロパティ、削除では残る必須プロパティ。
    pub(crate) added: Vec<RequiredProperty>,
    pub(crate) mode: MemberChangeMode,
}

pub(crate) enum MemberChangeMode {
    Addition,
    Narrowing,
    Removal(Vec<String>),
}

impl RequiredMemberDelta {
    fn audits_uses(&self) -> bool {
        !matches!(self.mode, MemberChangeMode::Addition)
    }

    fn removed_names(&self) -> &[String] {
        match &self.mode {
            MemberChangeMode::Removal(names) => names,
            _ => &[],
        }
    }
}

pub(crate) struct RequiredProperty {
    pub(crate) name: String,
    pub(crate) ty: Vec<TypeAtom>,
}

impl RequiredProperty {
    /// どんな値も代入できる型 (`any` / `unknown`) か。
    fn accepts_any_value(&self) -> bool {
        self.ty
            .iter()
            .any(|atom| matches!(atom, TypeAtom::Any | TypeAtom::Unknown))
    }
}

/// 型の構文を単純な型の union として読む。対応しない構文が 1 つでもあれば None。
pub(crate) fn simple_type(node: Node<'_>, source: &[u8]) -> Option<Vec<TypeAtom>> {
    simple_type_at(node, source, 0)
}

fn simple_type_at(node: Node<'_>, source: &[u8], depth: usize) -> Option<Vec<TypeAtom>> {
    if depth > MAX_ARRAY_DEPTH {
        return None;
    }
    let mut atoms = Vec::new();
    // union は要素数ぶん左に入れ子になるので、再帰せずに明示スタックで辿る。
    let mut stack = vec![node];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "union_type" | "parenthesized_type" => {
                let mut cursor = node.walk();
                stack.extend(
                    node.named_children(&mut cursor)
                        .filter(|child| child.kind() != "comment"),
                );
            }
            "predefined_type" => atoms.push(match node.utf8_text(source).ok()? {
                "string" => TypeAtom::String,
                "number" => TypeAtom::Number,
                "boolean" => TypeAtom::Boolean,
                "bigint" => TypeAtom::BigInt,
                "undefined" => TypeAtom::Undefined,
                "any" => TypeAtom::Any,
                "unknown" => TypeAtom::Unknown,
                _ => return None,
            }),
            "literal_type" => atoms.push(literal_atom(only_named_child(node)?, source)?),
            "array_type" => atoms.push(TypeAtom::Array(simple_type_at(
                only_named_child(node)?,
                source,
                depth + 1,
            )?)),
            _ => return None,
        }
    }
    atoms.sort();
    atoms.dedup();
    (!atoms.is_empty()).then_some(atoms)
}

/// literal 型・literal 式の 1 要素。
fn literal_atom(node: Node<'_>, source: &[u8]) -> Option<TypeAtom> {
    Some(match node.kind() {
        "string" => string_atom(node, source)?,
        "number" => number_atom(node.utf8_text(source).ok()?, ""),
        "unary_expression" => {
            if node.child_by_field_name("operator")?.kind() != "-" {
                return None;
            }
            let argument = node.child_by_field_name("argument")?;
            if argument.kind() != "number" {
                return None;
            }
            number_atom(argument.utf8_text(source).ok()?, "-")
        }
        "true" => TypeAtom::True,
        "false" => TypeAtom::False,
        "null" => TypeAtom::Null,
        "undefined" => TypeAtom::Undefined,
        _ => return None,
    })
}

fn string_atom(node: Node<'_>, source: &[u8]) -> Option<TypeAtom> {
    let inner = super::static_js_string_text(node, source)?;
    Some(TypeAtom::StrLit(
        (!inner.contains('\\')).then(|| inner.to_string()),
    ))
}

fn number_atom(text: &str, sign: &str) -> TypeAtom {
    if text.ends_with('n') {
        TypeAtom::BigIntLit(format!("{sign}{text}"))
    } else {
        TypeAtom::NumLit(format!("{sign}{text}"))
    }
}

/// 値の型 `value` が対象の型 `target` へ代入できるか (strictNullChecks を前提にする)。
pub(crate) fn assignable(value: &[TypeAtom], target: &[TypeAtom]) -> bool {
    if target
        .iter()
        .any(|atom| matches!(atom, TypeAtom::Any | TypeAtom::Unknown))
    {
        return true;
    }
    value.iter().all(|atom| atom_assignable(atom, target))
}

fn atom_assignable(value: &TypeAtom, target: &[TypeAtom]) -> bool {
    let has = |atom: &TypeAtom| target.contains(atom);
    match value {
        TypeAtom::Any => true,
        TypeAtom::Unknown => false,
        TypeAtom::StrLit(text) => {
            has(&TypeAtom::String) || (text.is_some() && has(&TypeAtom::StrLit(text.clone())))
        }
        TypeAtom::NumLit(text) => has(&TypeAtom::Number) || has(&TypeAtom::NumLit(text.clone())),
        TypeAtom::BigIntLit(text) => {
            has(&TypeAtom::BigInt) || has(&TypeAtom::BigIntLit(text.clone()))
        }
        TypeAtom::Boolean => {
            has(&TypeAtom::Boolean) || (has(&TypeAtom::True) && has(&TypeAtom::False))
        }
        TypeAtom::True | TypeAtom::False => has(&TypeAtom::Boolean) || has(value),
        TypeAtom::Array(elements) => target.iter().any(
            |atom| matches!(atom, TypeAtom::Array(expected) if assignable(elements, expected)),
        ),
        TypeAtom::String
        | TypeAtom::Number
        | TypeAtom::BigInt
        | TypeAtom::Null
        | TypeAtom::Undefined => has(value),
    }
}

/// 閉包判定の対象の型。
pub(super) struct ClosureScope<'a> {
    pub(super) dir: &'a str,
    pub(super) base: &'a str,
    /// 対象の型名 (bare)。
    pub(super) type_name: &'a str,
    /// 対象の型を宣言するファイル (dir 相対)。
    pub(super) target_path: &'a str,
}

/// 対象の型の参照 `refs` (定義・import・素通し再 export を除いたもの) が、すべて追随済みと
/// 証明できるか。1 件でも証明できなければ false。
pub(super) fn prove_closure(
    scope: &ClosureScope<'_>,
    refs: &[&SymbolReference],
    delta: &RequiredMemberDelta,
    diff_files: &mut DiffFileLookup<'_>,
    caches: &mut ApiClosureCaches,
) -> bool {
    let mut prover = Prover {
        scope,
        delta,
        consumers: Vec::new(),
        typeof_names: BTreeMap::new(),
        narrowed_paths: BTreeSet::new(),
        sites: 0,
    };
    for r in refs {
        if !prover.prove_reference(r, diff_files, caches) {
            return false;
        }
    }
    // 値を作る箇所も受け取る箇所も無い (公開するだけ) なら、利用者を 1 件も確かめていない。
    prover.sites > 0
        && prover.prove_deferred(diff_files, caches)
        && (!delta.audits_uses() || prover.prove_narrowed_uses(caches))
        && (delta.removed_names().is_empty() || prover.prove_removed_keys_absent(caches))
}

/// 受け取る箇所 (ファイル直下の関数の必須引数) の義務。呼び出しは関数名で後から検索する。
struct ConsumerSite {
    path: String,
    function: String,
    /// 関数名の行 (定義の照合に使う)。
    name_line: usize,
    /// 引数の位置。
    index: usize,
}

/// 型の参照 1 件の構文上の役割。
enum TypeRole<'t> {
    /// 関数・メソッドの戻り値の型。
    Return(Node<'t>),
    /// 変数の型注釈 (`variable_declarator`)。
    Variable(Node<'t>),
    /// `expr satisfies Row` の式全体。
    Satisfies(Node<'t>),
    /// 必須引数の型注釈 (`required_parameter`)。
    Parameter(Node<'t>),
}

/// import の束縛をどの名前空間で使うか。値として呼ぶ import は `import type` を受け付けない。
#[derive(Clone, Copy, PartialEq, Eq)]
enum ImportUse {
    Type,
    Value,
}

/// 直接呼び出しの参照 1 件の分類。
enum ConsumerRef {
    /// import・別名なしの export・素通し再 export。呼び出しではない。
    Neutral,
    /// 対象の型と証明できる値を渡す直接呼び出し。
    ProvenCall,
    /// 証明できない利用。
    Unproven,
}

struct Prover<'s, 'a> {
    scope: &'s ClosureScope<'a>,
    delta: &'s RequiredMemberDelta,
    consumers: Vec<ConsumerSite>,
    /// `typeof` で型を取り出されていないか確かめる名前 (値を作る関数・メソッド・変数と、値を
    /// 入れた外側の束縛)。値は、その名前が値を入れた入れ物 (`box`・`list`) か。
    typeof_names: BTreeMap<String, bool>,
    narrowed_paths: BTreeSet<String>,
    /// 証明した値を作る箇所・受け取る箇所の数。
    sites: usize,
}

impl Prover<'_, '_> {
    fn prove_reference(
        &mut self,
        r: &SymbolReference,
        diff_files: &mut DiffFileLookup<'_>,
        caches: &mut ApiClosureCaches,
    ) -> bool {
        let Some(parsed) = parsed_ref_file_shared(self.scope.dir, &r.path, caches) else {
            return false;
        };
        if !is_checked_source(&parsed, &r.path) {
            return false;
        }
        if self.delta.audits_uses() {
            self.narrowed_paths.insert(r.path.clone());
        }
        let source: &[u8] = &parsed.source;
        let Some(node) = ref_name_node(&parsed, self.scope.type_name, r.line, r.column) else {
            return false;
        };
        match node.kind() {
            "type_identifier" => {}
            "identifier"
                if node
                    .parent()
                    .is_some_and(|p| p.kind() == "export_specifier") =>
            {
                return plain_export_specifier(node, source);
            }
            // `import("./row").Row` は型の位置でもメンバー式になる。型の参照として証明しない。
            "property_identifier" if is_import_type_member(node) => return false,
            // 値の名前空間の出現 (同名の変数・関数・JSX の要素名・プロパティ名) は
            // interface / type alias を指さない。
            "identifier"
            | "property_identifier"
            | "shorthand_property_identifier"
            | "shorthand_property_identifier_pattern" => return true,
            _ => return false,
        }
        // `ns.Row` は名前空間の中の別の型かもしれない。
        if node
            .parent()
            .is_some_and(|p| p.kind() == "nested_type_identifier")
            || !self.binds_target_type(node, &parsed, &r.path, caches)
        {
            return false;
        }
        match type_role(node) {
            Some(TypeRole::Return(function)) => {
                self.prove_return(function, &parsed, &r.path, diff_files, caches)
            }
            Some(TypeRole::Variable(declarator)) => {
                self.prove_variable(declarator, &parsed, &r.path, diff_files, caches)
            }
            Some(TypeRole::Satisfies(satisfies)) => {
                if self.delta.audits_uses() {
                    return false;
                }
                let proven = first_named_child(satisfies).is_some_and(|expr| {
                    self.prove_value(expr, &parsed, &r.path, diff_files, caches)
                }) && self.register_producer(satisfies, &parsed);
                self.sites += usize::from(proven);
                proven
            }
            Some(TypeRole::Parameter(param)) => self.record_consumer(param, &parsed, &r.path),
            None => false,
        }
    }

    /// 型の位置の `node` (対象の型名) が対象の宣言へ束縛されるか。同じファイルの宣言か、
    /// 別名なしの named import で解決先がちょうど宣言ファイル 1 つのときだけ true。
    fn binds_target_type(
        &self,
        node: Node<'_>,
        parsed: &ParsedRefFile,
        path: &str,
        caches: &mut ApiClosureCaches,
    ) -> bool {
        match resolve_binding_scope(node, &parsed.source, self.scope.type_name, Namespace::Type) {
            Some((scope, ScopeBinding::TypeDeclaration)) => {
                scope.kind() == "program" && self.same_path(self.scope.target_path, path)
            }
            Some((_, ScopeBinding::Import(statement))) => self
                .import_target(
                    statement,
                    &parsed.source,
                    path,
                    self.scope.type_name,
                    ImportUse::Type,
                    caches,
                )
                .is_some_and(|resolved| self.same_path(self.scope.target_path, &resolved)),
            _ => false,
        }
    }

    /// 型注釈 `annotation` がちょうど対象の型か。
    fn annotation_is_target(
        &self,
        annotation: Node<'_>,
        parsed: &ParsedRefFile,
        path: &str,
        caches: &mut ApiClosureCaches,
    ) -> bool {
        only_named_child(annotation).is_some_and(|ty| {
            ty.kind() == "type_identifier"
                && ty.utf8_text(&parsed.source).ok() == Some(self.scope.type_name)
                && self.binds_target_type(ty, parsed, path, caches)
        })
    }

    /// 戻り値の型がちょうど対象の型の関数が、対象の型と証明できる値だけを返すか。
    fn prove_return(
        &mut self,
        function: Node<'_>,
        parsed: &ParsedRefFile,
        path: &str,
        diff_files: &mut DiffFileLookup<'_>,
        caches: &mut ApiClosureCaches,
    ) -> bool {
        if self.delta.audits_uses()
            && (function.kind() != "function_declaration" || !is_top_level_declaration(function))
        {
            return false;
        }
        if function
            .child_by_field_name("return_type")
            .and_then(only_named_child)
            .is_none_or(|ty| ty.kind() != "type_identifier")
        {
            if self.delta.audits_uses() {
                return false;
            }
            return self.prove_wrapped_return(function, parsed, path, diff_files, caches);
        }
        if has_token(function, "async") || has_token(function, "*") {
            return false;
        }
        let Some(body) = function.child_by_field_name("body") else {
            return false;
        };
        if self.delta.audits_uses() && !return_producers::definitely_exits(body, 0) {
            return false;
        }
        let proven = if body.kind() == "statement_block" {
            let returns = function_returns(body);
            !returns.is_empty()
                && returns.into_iter().all(|ret| {
                    only_named_child(ret).is_some_and(|value| {
                        self.prove_value(value, parsed, path, diff_files, caches)
                    })
                })
        } else {
            // 式本体の arrow (`(): Row => ({ ... })`)。
            self.prove_value(body, parsed, path, diff_files, caches)
        };
        if !proven || !self.register_producer(function, parsed) {
            return false;
        }
        self.sites += 1;
        true
    }

    /// 値を作る箇所の値を束縛する名前を、`typeof` の確認の対象に加える。default export や
    /// 別名の export のように名前で利用者を追えない場合は false (証明しない)。
    fn register_producer(&mut self, node: Node<'_>, parsed: &ParsedRefFile) -> bool {
        if matches!(self.delta.mode, MemberChangeMode::Addition)
            && let Some(proven) = local_producers::prove_local_variable(node, parsed)
        {
            return proven;
        }
        let source: &[u8] = &parsed.source;
        match producer_binding(node, source) {
            ProducerName::Anonymous => true,
            ProducerName::Hidden => false,
            ProducerName::Named(names) => {
                let published = super::js_public_name::published_names(&[source], parsed.lang_id);
                if names.iter().any(|(name, _)| published.may_differ(name)) {
                    return false;
                }
                for (name, container) in names {
                    *self.typeof_names.entry(name).or_insert(false) |= container;
                }
                true
            }
        }
    }

    /// `const x: Row = <値>` の値が対象の型と証明できるか。再代入できる `let` / `var` と、
    /// 値を持たない宣言 (`declare const`) は証明しない。
    fn prove_variable(
        &mut self,
        declarator: Node<'_>,
        parsed: &ParsedRefFile,
        path: &str,
        diff_files: &mut DiffFileLookup<'_>,
        caches: &mut ApiClosureCaches,
    ) -> bool {
        if !super::ts_const_arg::declarator_is_const(declarator) {
            return false;
        }
        let Some(value) = declarator.child_by_field_name("value") else {
            return false;
        };
        if !self.prove_value(value, parsed, path, diff_files, caches)
            || !self.register_producer(declarator, parsed)
        {
            return false;
        }
        self.sites += 1;
        true
    }

    /// 必須引数の型注釈を、呼び出し側の確認待ちの義務として記録する。ファイル直下の
    /// 関数宣言の、既定値の無い単純な識別子の引数で、本体が引数へ書き込まない場合だけ。
    fn record_consumer(&mut self, param: Node<'_>, parsed: &ParsedRefFile, path: &str) -> bool {
        let source: &[u8] = &parsed.source;
        let Some(pattern) = param
            .child_by_field_name("pattern")
            .filter(|p| p.kind() == "identifier")
        else {
            return false;
        };
        if param.child_by_field_name("value").is_some() {
            return false;
        }
        let Some(formal) = param.parent().filter(|p| p.kind() == "formal_parameters") else {
            return false;
        };
        let Some(function) = formal
            .parent()
            .filter(|f| f.kind() == "function_declaration" && is_top_level_declaration(*f))
        else {
            return false;
        };
        let Some(name_node) = function.child_by_field_name("name") else {
            return false;
        };
        if self.delta.audits_uses() && function.child_by_field_name("return_type").is_none() {
            return false;
        }
        let (Ok(function_name), Ok(param_name)) =
            (name_node.utf8_text(source), pattern.utf8_text(source))
        else {
            return false;
        };
        let mut cursor = formal.walk();
        let params: Vec<Node<'_>> = formal
            .named_children(&mut cursor)
            .filter(|p| p.kind() != "comment")
            .collect();
        // `this` 引数は呼び出しの実引数に現れず、位置がずれる。
        if params.iter().any(|p| {
            p.child_by_field_name("pattern")
                .is_some_and(|pattern| pattern.kind() == "this")
        }) {
            return false;
        }
        let Some(index) = params.iter().position(|p| p.id() == param.id()) else {
            return false;
        };
        // overload の宣言や同名の束縛があれば、呼び出しの解決先が曖昧になる。
        let root = parsed.tree.root_node();
        if super::ts_const_arg::collect_bindings_named(root, source, function_name).len() != 1 {
            return false;
        }
        // default export / 別名 export の利用者は関数名の参照に現れない。
        if super::js_public_name::published_names(&[source], parsed.lang_id)
            .may_differ(function_name)
        {
            return false;
        }
        let Some(body) = function.child_by_field_name("body") else {
            return false;
        };
        if body_rebinds_or_queries(body, source, param_name) {
            return false;
        }
        self.consumers.push(ConsumerSite {
            path: path.to_string(),
            function: function_name.to_string(),
            name_line: name_node.start_position().row,
            index,
        });
        self.sites += 1;
        true
    }

    /// 関数名で集める参照が要る義務 (呼び出し側・`typeof`) をまとめて確かめる。
    fn prove_deferred(
        &self,
        diff_files: &mut DiffFileLookup<'_>,
        caches: &mut ApiClosureCaches,
    ) -> bool {
        let mut names: BTreeSet<&str> = self.typeof_names.keys().map(String::as_str).collect();
        // `.vue` などの解析できない埋め込みソースの `typeof` は名前の参照に現れない。
        let producer_names: Vec<&str> = names.iter().copied().collect();
        if embedded_js_sources_may_mention(self.scope.dir, &producer_names, caches) {
            return false;
        }
        names.extend(self.consumers.iter().map(|c| c.function.as_str()));
        ensure_name_refs(self.scope.dir, &names, caches);
        for (name, &container) in &self.typeof_names {
            let Some(refs) = name_refs(caches, name) else {
                return false;
            };
            for r in refs.iter().filter(|r| r.kind != Some(RefKind::Definition)) {
                // 別名の import・再 export (`import { toRow as make }`) の先の `typeof make` は、
                // 元の名前の参照に現れない。
                if !matches!(
                    ref_import_role(self.scope.dir, r, name, caches),
                    Some(RefImportRole::Plain | RefImportRole::Outside)
                ) || !self.reference_outside_type_query(r, name, container, caches)
                {
                    return false;
                }
            }
        }
        if !self
            .consumers
            .iter()
            .all(|consumer| self.prove_consumer_calls(consumer, diff_files, caches))
        {
            return false;
        }
        // JSDoc と、文字列で書いた名前 (`rows["toRow"]`) は名前の参照に現れない。最後に、
        // 走査の要る確認をする。
        let string_names: Vec<&str> = names.iter().copied().collect();
        names.insert(self.scope.type_name);
        let comment_names: Vec<&str> = names.into_iter().collect();
        !sources_may_mention_outside_identifiers(
            self.scope.dir,
            &comment_names,
            &string_names,
            caches,
        )
    }

    /// 参照が `typeof name` (型の取り出し) の中に無いと確かめられるか。JS / TS 以外の
    /// ファイルの参照は `typeof` になり得ない。
    fn reference_outside_type_query(
        &self,
        r: &SymbolReference,
        name: &str,
        container: bool,
        caches: &mut ApiClosureCaches,
    ) -> bool {
        if !matches!(
            LangId::from_path(camino::Utf8Path::new(&r.path)),
            Ok(LangId::Javascript | LangId::Typescript | LangId::Tsx)
        ) {
            return true;
        }
        let Some(parsed) = parsed_ref_file_shared(self.scope.dir, &r.path, caches) else {
            return false;
        };
        if parsed.tree.root_node().has_error() {
            return false;
        }
        let Some(node) = ref_name_node(&parsed, name, r.line, r.column) else {
            return false;
        };
        node_outside_type_query(node, &parsed.source, container)
    }

    /// 受け取る関数の全参照が、import・export か、対象の型と証明できる値を渡す直接呼び出し
    /// であること。直接呼び出しが 1 件も無ければ (外部の利用者しかいなければ) 証明しない。
    fn prove_consumer_calls(
        &self,
        consumer: &ConsumerSite,
        diff_files: &mut DiffFileLookup<'_>,
        caches: &mut ApiClosureCaches,
    ) -> bool {
        let Some(refs) = name_refs(caches, &consumer.function) else {
            return false;
        };
        let defs: Vec<&SymbolReference> = refs
            .iter()
            .filter(|r| r.kind == Some(RefKind::Definition))
            .collect();
        let [def] = defs.as_slice() else {
            return false;
        };
        if !self.same_path(&consumer.path, &def.path) || def.line != consumer.name_line {
            return false;
        }
        if embedded_js_sources_may_mention(self.scope.dir, &[consumer.function.as_str()], caches) {
            return false;
        }
        let mut calls = 0usize;
        for r in refs.iter().filter(|r| r.kind != Some(RefKind::Definition)) {
            match self.classify_consumer_reference(consumer, r, diff_files, caches) {
                ConsumerRef::Neutral => {}
                ConsumerRef::ProvenCall => calls += 1,
                ConsumerRef::Unproven => return false,
            }
        }
        calls > 0
    }

    fn classify_consumer_reference(
        &self,
        consumer: &ConsumerSite,
        r: &SymbolReference,
        diff_files: &mut DiffFileLookup<'_>,
        caches: &mut ApiClosureCaches,
    ) -> ConsumerRef {
        let dir = self.scope.dir;
        let name = consumer.function.as_str();
        match ref_import_role(dir, r, name, caches) {
            Some(RefImportRole::Plain) => return ConsumerRef::Neutral,
            Some(RefImportRole::Outside) => {}
            Some(RefImportRole::Aliased) | None => return ConsumerRef::Unproven,
        }
        if ref_is_pass_through_reexport(dir, r, name, caches) {
            return ConsumerRef::Neutral;
        }
        let Some(parsed) = parsed_ref_file_shared(dir, &r.path, caches) else {
            return ConsumerRef::Unproven;
        };
        if !is_checked_source(&parsed, &r.path) {
            return ConsumerRef::Unproven;
        }
        let source: &[u8] = &parsed.source;
        let Some(node) = ref_name_node(&parsed, name, r.line, r.column) else {
            return ConsumerRef::Unproven;
        };
        if node
            .parent()
            .is_some_and(|p| p.kind() == "export_specifier")
        {
            return if plain_export_specifier(node, source) {
                ConsumerRef::Neutral
            } else {
                ConsumerRef::Unproven
            };
        }
        if is_intrinsic_jsx_name(node, source) {
            return ConsumerRef::Neutral;
        }
        let Some(call) = direct_call(node) else {
            return ConsumerRef::Unproven;
        };
        if !self.callee_is_consumer(node, &parsed, &r.path, consumer, caches) {
            return ConsumerRef::Unproven;
        }
        let Some(arguments) = call
            .child_by_field_name("arguments")
            .filter(|a| a.kind() == "arguments")
        else {
            return ConsumerRef::Unproven;
        };
        let mut cursor = arguments.walk();
        let args: Vec<Node<'_>> = arguments
            .named_children(&mut cursor)
            .filter(|a| a.kind() != "comment")
            .collect();
        // spread があると実引数と仮引数の位置の対応が崩れる。
        if args
            .iter()
            .take(consumer.index + 1)
            .any(|a| a.kind() == "spread_element")
        {
            return ConsumerRef::Unproven;
        }
        match args.get(consumer.index) {
            Some(arg) if self.prove_value(*arg, &parsed, &r.path, diff_files, caches) => {
                ConsumerRef::ProvenCall
            }
            _ => ConsumerRef::Unproven,
        }
    }

    /// 呼び出しの callee が受け取る関数そのものへ束縛されるか。
    fn callee_is_consumer(
        &self,
        callee: Node<'_>,
        parsed: &ParsedRefFile,
        path: &str,
        consumer: &ConsumerSite,
        caches: &mut ApiClosureCaches,
    ) -> bool {
        match resolve_binding_scope(callee, &parsed.source, &consumer.function, Namespace::Value) {
            Some((scope, ScopeBinding::FunctionDeclaration)) => {
                scope.kind() == "program" && self.same_path(&consumer.path, path)
            }
            Some((_, ScopeBinding::Import(statement))) => self
                .import_target(
                    statement,
                    &parsed.source,
                    path,
                    &consumer.function,
                    ImportUse::Value,
                    caches,
                )
                .is_some_and(|resolved| self.same_path(&consumer.path, &resolved)),
            _ => false,
        }
    }

    /// 式 `expr` の値が対象の型と証明できるか。
    fn prove_value(
        &self,
        expr: Node<'_>,
        parsed: &ParsedRefFile,
        path: &str,
        diff_files: &mut DiffFileLookup<'_>,
        caches: &mut ApiClosureCaches,
    ) -> bool {
        // 括弧と、ちょうど対象の型への `satisfies` だけを剥がす (文脈の型が同じなので、中の
        // literal の型付けは変わらない)。別の型への `satisfies` と `as` (`as const` を含む) は
        // 中の値の型を変えるので証明しない。
        let mut value = expr;
        for _ in 0..MAX_UNWRAP {
            let Some(inner) = unwrap_parens(value) else {
                return false;
            };
            value = inner;
            if value.kind() != "satisfies_expression" {
                break;
            }
            let mut cursor = value.walk();
            let operands: Vec<Node<'_>> = value
                .named_children(&mut cursor)
                .filter(|n| n.kind() != "comment")
                .collect();
            match operands.as_slice() {
                [inner, ty]
                    if ty.kind() == "type_identifier"
                        && ty.utf8_text(&parsed.source).ok() == Some(self.scope.type_name)
                        && self.binds_target_type(*ty, parsed, path, caches) =>
                {
                    value = *inner;
                }
                _ => return false,
            }
        }
        match value.kind() {
            "object" => self.prove_object_literal(value, parsed, path, diff_files, caches),
            "call_expression" => self.call_returns_target(value, parsed, path, caches),
            "identifier" => self.identifier_has_target_type(value, parsed, path, caches),
            _ => false,
        }
    }

    /// object literal が検査対象の必須プロパティを 1 つずつ持ち、値の型が適合するか。
    /// 追加・縮小ではその行の変更も求め、削除では既に適合する未変更の値も許す。
    fn prove_object_literal(
        &self,
        object: Node<'_>,
        parsed: &ParsedRefFile,
        path: &str,
        diff_files: &mut DiffFileLookup<'_>,
        caches: &mut ApiClosureCaches,
    ) -> bool {
        let source: &[u8] = &parsed.source;
        let changed = if matches!(self.delta.mode, MemberChangeMode::Removal(_)) {
            None
        } else {
            let Some(changed) = changed_lines_for_ref_path(
                self.scope.dir,
                self.scope.base,
                path,
                diff_files,
                caches,
            ) else {
                return false;
            };
            Some(changed)
        };
        let names: HashSet<&str> = self.delta.added.iter().map(|p| p.name.as_str()).collect();
        // escape を含む名前 (`n\u006fte`・`"n\u006fte"`) は、表記と違う名前のキーを作る。
        // 名前を正規化しないので、どのキーでも比べられない。
        let escaped = |key: Node<'_>| key.utf8_text(source).is_ok_and(|text| text.contains('\\'));
        let mut found: HashMap<&str, Vec<(Node<'_>, usize)>> = HashMap::new();
        let mut cursor = object.walk();
        for member in object.named_children(&mut cursor) {
            match member.kind() {
                "comment" => {}
                "pair" => {
                    let (Some(key), Some(value)) = (
                        member.child_by_field_name("key"),
                        member.child_by_field_name("value"),
                    ) else {
                        return false;
                    };
                    if key.kind() == "computed_property_name" || escaped(key) {
                        return false;
                    }
                    if let Some(key) = super::object_key_text(key, source)
                        && let Some(name) = names.get(key.as_str())
                    {
                        found
                            .entry(name)
                            .or_default()
                            .push((value, member.start_position().row));
                    }
                }
                "shorthand_property_identifier" => {
                    if escaped(member) {
                        return false;
                    }
                    if let Some(name) = member
                        .utf8_text(source)
                        .ok()
                        .and_then(|text| names.get(text))
                    {
                        found
                            .entry(name)
                            .or_default()
                            .push((member, member.start_position().row));
                    }
                }
                // メソッド・getter・setter が追加したキーを作る形と、名前を静的に決められない形は
                // 値の型を確かめられない。
                "method_definition" => {
                    if self.delta.audits_uses() {
                        return false;
                    }
                    let Some(key) = member.child_by_field_name("name") else {
                        return false;
                    };
                    if key.kind() == "computed_property_name"
                        || escaped(key)
                        || super::object_key_text(key, source)
                            .is_some_and(|key| names.contains(key.as_str()))
                    {
                        return false;
                    }
                }
                // spread はキーと値の型を静的に決められない。
                _ => return false,
            }
        }
        self.delta.added.iter().all(|property| {
            let Some([(value, line)]) = found.get(property.name.as_str()).map(Vec::as_slice) else {
                return false;
            };
            (matches!(self.delta.mode, MemberChangeMode::Removal(_))
                || changed.as_ref().is_some_and(|lines| lines.contains(line)))
                && (property.accepts_any_value()
                    || self
                        .value_type(*value, parsed, 0)
                        .is_some_and(|value_type| {
                            assignable(&value_type, &property.ty)
                                && (!self.delta.audits_uses()
                                    || self.narrowed_value_sources(
                                        *value,
                                        parsed,
                                        0,
                                        &mut HashSet::new(),
                                    ))
                        }))
        })
    }

    /// `f(...)` の `f` が、戻り値の型がちょうど対象の型のファイル直下の関数宣言か。
    fn call_returns_target(
        &self,
        call: Node<'_>,
        parsed: &ParsedRefFile,
        path: &str,
        caches: &mut ApiClosureCaches,
    ) -> bool {
        // `f?.()` の結果は undefined になり得る。
        if call.child_by_field_name("optional_chain").is_some() {
            return false;
        }
        let Some(callee) = call
            .child_by_field_name("function")
            .filter(|c| c.kind() == "identifier")
        else {
            return false;
        };
        let Ok(name) = callee.utf8_text(&parsed.source) else {
            return false;
        };
        match resolve_binding_scope(callee, &parsed.source, name, Namespace::Value) {
            Some((scope, ScopeBinding::FunctionDeclaration)) if scope.kind() == "program" => {
                self.top_level_function_returns_target(parsed, path, name, false, caches)
            }
            Some((_, ScopeBinding::Import(statement))) => {
                let Some(resolved) = self.import_target(
                    statement,
                    &parsed.source,
                    path,
                    name,
                    ImportUse::Value,
                    caches,
                ) else {
                    return false;
                };
                let Some(target) = parsed_ref_file_shared(self.scope.dir, &resolved, caches) else {
                    return false;
                };
                is_checked_source(&target, &resolved)
                    && self
                        .top_level_function_returns_target(&target, &resolved, name, true, caches)
            }
            _ => false,
        }
    }

    /// ファイル直下に `name` の関数宣言がちょうど 1 つあり、戻り値の型がちょうど対象の型か。
    /// `require_export` なら同じ名前で export されていること (別ファイルから import した場合)。
    fn top_level_function_returns_target(
        &self,
        parsed: &ParsedRefFile,
        path: &str,
        name: &str,
        require_export: bool,
        caches: &mut ApiClosureCaches,
    ) -> bool {
        let source: &[u8] = &parsed.source;
        let root = parsed.tree.root_node();
        let mut matches = Vec::new();
        let mut exported_by_clause = false;
        let mut cursor = root.walk();
        for statement in root.named_children(&mut cursor) {
            match statement.kind() {
                "function_declaration" if super::node_field_name_eq(statement, name, source) => {
                    matches.push((statement, false));
                }
                "export_statement" if !has_token(statement, "default") => {
                    if let Some(decl) = statement
                        .child_by_field_name("declaration")
                        .filter(|d| d.kind() == "function_declaration")
                    {
                        if super::node_field_name_eq(decl, name, source) {
                            matches.push((decl, true));
                        }
                    } else if statement.child_by_field_name("source").is_none() {
                        exported_by_clause |= any_node(statement, Children::Named, |n| {
                            n.kind() == "export_specifier"
                                && n.child_by_field_name("name")
                                    .is_some_and(|spec| plain_export_specifier(spec, source))
                                && n.child_by_field_name("name")
                                    .and_then(|spec| spec.utf8_text(source).ok())
                                    == Some(name)
                        });
                    }
                }
                _ => {}
            }
        }
        let [(function, exported)] = matches.as_slice() else {
            return false;
        };
        if require_export && !exported && !exported_by_clause {
            return false;
        }
        // overload の宣言や同名の束縛があれば、呼び出しの解決先が曖昧になる。
        if super::ts_const_arg::collect_bindings_named(root, source, name).len() != 1
            || has_token(*function, "async")
        {
            return false;
        }
        function
            .child_by_field_name("return_type")
            .is_some_and(|annotation| self.annotation_is_target(annotation, parsed, path, caches))
    }

    /// 識別子の束縛の型注釈がちょうど対象の型か (必須引数・変数)。注釈の付いた束縛自体は
    /// 対象の型の参照として別に証明する。
    fn identifier_has_target_type(
        &self,
        ident: Node<'_>,
        parsed: &ParsedRefFile,
        path: &str,
        caches: &mut ApiClosureCaches,
    ) -> bool {
        let Ok(name) = ident.utf8_text(&parsed.source) else {
            return false;
        };
        match resolve_binding_scope(ident, &parsed.source, name, Namespace::Value) {
            Some((_, ScopeBinding::Variable(declarator))) => {
                declarator
                    .child_by_field_name("name")
                    .is_some_and(|n| n.kind() == "identifier")
                    && declarator
                        .child_by_field_name("type")
                        .is_some_and(|ann| self.annotation_is_target(ann, parsed, path, caches))
            }
            Some((scope, ScopeBinding::Other)) => parameter_named(scope, &parsed.source, name)
                .is_some_and(|param| {
                    param.kind() == "required_parameter"
                        && param
                            .child_by_field_name("type")
                            .is_some_and(|ann| self.annotation_is_target(ann, parsed, path, caches))
                }),
            _ => false,
        }
    }

    /// プロパティの値 `expr` の型。対応しない式は None。
    fn value_type(
        &self,
        expr: Node<'_>,
        parsed: &ParsedRefFile,
        depth: usize,
    ) -> Option<Vec<TypeAtom>> {
        let source: &[u8] = &parsed.source;
        let value = unwrap_literal_value(expr)?;
        let atom = match value.kind() {
            "template_string" => {
                let mut cursor = value.walk();
                if value
                    .named_children(&mut cursor)
                    .any(|child| child.kind() == "template_substitution")
                {
                    TypeAtom::String
                } else {
                    string_atom(value, source)?
                }
            }
            "array" => return self.array_type(value, parsed, depth),
            "undefined" => return undefined_type(parsed),
            "identifier" | "shorthand_property_identifier" => {
                if value.utf8_text(source).ok()? == "undefined" {
                    return undefined_type(parsed);
                }
                return self.binding_value_type(value, parsed);
            }
            _ => literal_atom(value, source)?,
        };
        Some(vec![atom])
    }

    fn array_type(
        &self,
        array: Node<'_>,
        parsed: &ParsedRefFile,
        depth: usize,
    ) -> Option<Vec<TypeAtom>> {
        if depth >= MAX_ARRAY_DEPTH {
            return None;
        }
        // 穴 (`[a, , b]`) は undefined の要素になる。位置を数えられないので扱わない。
        let mut expect_element = true;
        let mut atoms = Vec::new();
        let mut cursor = array.walk();
        for child in array.children(&mut cursor) {
            match child.kind() {
                "[" | "]" | "comment" => {}
                "," => {
                    if expect_element {
                        return None;
                    }
                    expect_element = true;
                }
                "spread_element" => return None,
                _ => {
                    atoms.extend(self.value_type(child, parsed, depth + 1)?);
                    expect_element = false;
                }
            }
        }
        atoms.sort();
        atoms.dedup();
        Some(vec![TypeAtom::Array(atoms)])
    }

    /// 識別子の束縛の型。型注釈があればそれ、無ければ const の primitive literal の literal 型
    /// (`let` / `var` と object・配列の初期化子は widening があるので扱わない)。
    fn binding_value_type(&self, ident: Node<'_>, parsed: &ParsedRefFile) -> Option<Vec<TypeAtom>> {
        let source: &[u8] = &parsed.source;
        let name = ident.utf8_text(source).ok()?;
        match resolve_binding_scope(ident, source, name, Namespace::Value)? {
            (_, ScopeBinding::Variable(declarator)) => {
                if declarator.child_by_field_name("name")?.kind() != "identifier" {
                    return None;
                }
                if let Some(annotation) = declarator.child_by_field_name("type") {
                    return simple_type(only_named_child(annotation)?, source);
                }
                if !super::ts_const_arg::declarator_is_const(declarator) {
                    return None;
                }
                let init = unwrap_literal_value(declarator.child_by_field_name("value")?)?;
                if !is_primitive_literal(init) {
                    return None;
                }
                self.value_type(init, parsed, MAX_ARRAY_DEPTH)
            }
            (scope, ScopeBinding::Other) => {
                let Some(param) = parameter_named(scope, source, name) else {
                    return self.map_parameter_type(scope, name, parsed);
                };
                if param.child_by_field_name("type").is_none() {
                    return self.map_parameter_type(scope, name, parsed);
                }
                let annotation = param.child_by_field_name("type")?;
                let mut ty = simple_type(only_named_child(annotation)?, source)?;
                if param.kind() == "optional_parameter" {
                    ty.push(TypeAtom::Undefined);
                    ty.sort();
                    ty.dedup();
                }
                Some(ty)
            }
            _ => None,
        }
    }

    /// 相対 import の束縛 `name` の解決先を 1 ファイルに決める。別名・default・namespace の
    /// import、存在する解決候補が複数か 0、解決を変える設定 (rootDirs / moduleSuffixes) が
    /// あり得る場合は None。
    fn import_target(
        &self,
        statement: Node<'_>,
        source: &[u8],
        from_path: &str,
        name: &str,
        import_use: ImportUse,
        caches: &mut ApiClosureCaches,
    ) -> Option<String> {
        if import_use == ImportUse::Value && has_token(statement, "type") {
            return None;
        }
        if !named_import_specifier(statement, source, name, import_use) {
            return None;
        }
        let spec = super::static_js_string_text(statement.child_by_field_name("source")?, source)?;
        let candidates = super::removed_attribution::relative_import_candidates(from_path, spec)?;
        let mut existing: Vec<&String> = candidates
            .iter()
            .filter(|candidate| Path::new(self.scope.dir).join(candidate).is_file())
            .collect();
        existing.sort();
        let [only] = existing.as_slice() else {
            return None;
        };
        relative_resolution_is_plain(self.scope.dir, from_path, caches).then(|| (*only).clone())
    }

    fn same_path(&self, a: &str, b: &str) -> bool {
        diff_path_matches_ref(a, b, self.scope.dir)
    }
}

fn node_outside_type_query(node: Node<'_>, source: &[u8], container: bool) -> bool {
    let mut current = node;
    for _ in 0..MAX_TYPE_QUERY_CHAIN {
        let Some(parent) = current.parent() else {
            return true;
        };
        match parent.kind() {
            "type_query" => return false,
            "member_expression" | "nested_identifier" | "subscript_expression" => current = parent,
            _ => break,
        }
    }
    !binds_value_to_another_name(node, source, container)
}

/// 型の参照 `node` (type_identifier) の構文上の役割。型注釈の中身がちょうどその名前の
/// 場合だけ (`Row | null` や `Row[]` は対象外)。
fn type_role(node: Node<'_>) -> Option<TypeRole<'_>> {
    let parent = node.parent()?;
    if parent.kind() == "satisfies_expression" {
        let mut cursor = parent.walk();
        let operands: Vec<Node<'_>> = parent
            .named_children(&mut cursor)
            .filter(|n| n.kind() != "comment")
            .collect();
        return match operands.as_slice() {
            [_, ty] if ty.id() == node.id() => Some(TypeRole::Satisfies(parent)),
            _ => None,
        };
    }
    if parent.kind() != "type_annotation"
        || only_named_child(parent).map(|n| n.id()) != Some(node.id())
    {
        return return_producers::wrapped_return_owner(node).map(TypeRole::Return);
    }
    let owner = parent.parent()?;
    let field_is = |field: &str| {
        owner
            .child_by_field_name(field)
            .is_some_and(|child| child.id() == parent.id())
    };
    match owner.kind() {
        "function_declaration"
        | "function_expression"
        | "function"
        | "arrow_function"
        | "method_definition"
            if field_is("return_type") =>
        {
            Some(TypeRole::Return(owner))
        }
        "variable_declarator" if field_is("type") => Some(TypeRole::Variable(owner)),
        "required_parameter" if field_is("type") => Some(TypeRole::Parameter(owner)),
        _ => None,
    }
}

/// 証明に使える JS / TS のファイルか (構文エラーと宣言ファイルを除く)。
fn is_checked_source(parsed: &ParsedRefFile, path: &str) -> bool {
    matches!(
        parsed.lang_id,
        LangId::Javascript | LangId::Typescript | LangId::Tsx
    ) && !parsed.tree.root_node().has_error()
        && ![".d.ts", ".d.mts", ".d.cts"]
            .iter()
            .any(|suffix| path.ends_with(suffix))
}

/// 別名を付けない export 指定子の名前か (`export { Row }`)。
fn plain_export_specifier(node: Node<'_>, source: &[u8]) -> bool {
    let Some(specifier) = node.parent().filter(|p| p.kind() == "export_specifier") else {
        return false;
    };
    specifier
        .child_by_field_name("name")
        .is_some_and(|name| name.id() == node.id())
        && specifier
            .child_by_field_name("alias")
            .is_none_or(|alias| alias.utf8_text(source).ok() == node.utf8_text(source).ok())
}

/// `import("./row").Row` (import 型) のメンバー名か。tree-sitter は型の位置でも
/// `member_expression` で表す。
fn is_import_type_member(node: Node<'_>) -> bool {
    let Some(member) = node.parent().filter(|p| {
        p.kind() == "member_expression"
            && p.child_by_field_name("property")
                .is_some_and(|property| property.id() == node.id())
    }) else {
        return false;
    };
    let mut object = member.child_by_field_name("object");
    for _ in 0..MAX_TYPE_QUERY_CHAIN {
        let Some(current) = object else {
            return false;
        };
        match current.kind() {
            "member_expression" => object = current.child_by_field_name("object"),
            "parenthesized_expression" => object = first_named_child(current),
            "call_expression" => {
                return current
                    .child_by_field_name("function")
                    .is_some_and(|function| function.kind() == "import");
            }
            _ => return false,
        }
    }
    // 連鎖が長すぎて起点を確かめられない。型の参照かもしれないので証明しない側へ倒す。
    true
}

/// 値を作る関数・変数の名前の出現 `node` が、その値を別の名前へ束縛するか、オブジェクト・
/// 配列へ入れるか (`const make = toRow`・`{ make: toRow }`・`[toRow]`・`{ ...SAMPLE }`・
/// `const { toRow: make } = mod`)。その先の `typeof make` / `typeof box` は元の名前の参照に
/// 現れない。呼び出し・引数・メンバーの読み出しは、推論で型が伝わるだけなので対象外。
fn binds_value_to_another_name(node: Node<'_>, source: &[u8], container: bool) -> bool {
    match node.kind() {
        // `{ toRow }` は値をオブジェクトへ入れる。
        "shorthand_property_identifier" => return true,
        // `const { toRow } = mod` は同じ名前で束縛するので、その先も名前で見つかる。
        "shorthand_property_identifier_pattern" => return false,
        // `const { toRow: make } = mod` は別の名前で束縛し直す。
        "property_identifier" | "identifier"
            if node.parent().is_some_and(|p| {
                p.kind() == "pair_pattern"
                    && p.child_by_field_name("key")
                        .is_some_and(|key| key.id() == node.id())
            }) =>
        {
            return true;
        }
        _ => {}
    }
    // 値がそのまま伝わる式 (括弧・型の付け外し・条件式の分岐・`&&` / `||` / `??`・カンマ式の
    // 最後・`await`) と、メンバー・要素の読み出し (`box.item`・`list[0]`) を上って、値が
    // 置かれる位置を見る。
    let mut current = node;
    for _ in 0..MAX_UNWRAP {
        let Some(parent) = current.parent() else {
            return false;
        };
        let field_is = |field: &str| {
            parent
                .child_by_field_name(field)
                .is_some_and(|child| child.id() == current.id())
        };
        // メンバー名の位置 (`m.toRow`・`box.item`) は上る。オブジェクトの位置は、値を入れた
        // 入れ物 (`box`・`list`) のときだけ上る (中身を読み出すので)。値そのもの (`row.id`) の
        // メンバーの読み出しは、元の値を写さない。
        if value_flows_to_parent(current, parent)
            || (matches!(parent.kind(), "member_expression" | "subscript_expression")
                && (field_is("property") || (container && field_is("object"))))
        {
            current = parent;
            continue;
        }
        return match parent.kind() {
            "variable_declarator" => {
                field_is("value")
                    && parent
                        .child_by_field_name("name")
                        .is_none_or(|name| binds_whole_value(name, source, container))
            }
            "assignment_expression" | "augmented_assignment_expression" => {
                field_is("right")
                    && parent
                        .child_by_field_name("left")
                        .is_none_or(|left| binds_whole_value(left, source, container))
            }
            "public_field_definition" | "field_definition" | "pair" => field_is("value"),
            // 要素を別の名前へ束縛する (`for (const r of list)`)・入れ物に入れる・既定値にする。
            "array"
            | "spread_element"
            | "for_in_statement"
            | "assignment_pattern"
            | "object_assignment_pattern" => true,
            // `export default toRow` は任意の名前で import される。
            "export_statement" => parent.child_by_field_name("value").is_some(),
            // 値を呼ぶ・渡す・返す・条件に使う・メンバーを読み出すだけの位置 (推論で型が伝わる
            // 範囲)。入れ物のメンバーの読み出しは上で上っている。
            "member_expression"
            | "subscript_expression"
            | "call_expression"
            | "new_expression"
            | "arguments"
            | "return_statement"
            | "expression_statement"
            | "arrow_function"
            | "unary_expression"
            | "binary_expression"
            | "ternary_expression"
            | "sequence_expression"
            | "if_statement"
            | "while_statement"
            | "do_statement"
            | "for_statement"
            | "switch_statement"
            | "template_substitution"
            | "jsx_expression"
            | "throw_statement"
            | "export_specifier"
            | "import_specifier" => false,
            // 知らない構文は、値を別の名前へ渡すかもしれないので束縛とみなす。
            _ => true,
        };
    }
    true
}

/// 代入・分割代入の左辺が値の全体 (入れ物なら中身) を束縛するか。識別子と rest
/// (`{ ...rest }`) を含むパターンは全体 (またはその残り) を写す。メンバーを取り出すだけの
/// パターンは写さない。
///
/// 入れ物の経路上のキー名は確かめているので、識別子のキーで取り出す分割代入
/// (`{ item }`・`{ item: renamed }`) はそのキー名の参照で見つかる。名前の無い位置
/// (`const [first] = list`) や、識別子で書いていないキー (`{ "item": picked }`・`{ 0: picked }`・
/// `{ [key]: picked }`・escape を含む名前) で取り出すと、別の名前への束縛が名前の参照に現れない。
fn binds_whole_value(target: Node<'_>, source: &[u8], container: bool) -> bool {
    match target.kind() {
        "array_pattern" if container => true,
        "object_pattern" | "array_pattern" => any_node(target, Children::Named, |node| {
            node.kind() == "rest_pattern"
                || (container
                    && node.kind() == "pair_pattern"
                    && node.child_by_field_name("key").is_none_or(|key| {
                        !matches!(key.kind(), "property_identifier" | "identifier")
                            || key.utf8_text(source).is_ok_and(|text| text.contains('\\'))
                    }))
        }),
        _ => true,
    }
}

/// `current` の値が、そのまま親の式の値になるか (括弧・型の付け外し・`await`・条件式の分岐・
/// `&&` / `||` / `??` の両辺・カンマ式の最後の式)。条件の部分は値を伝えない。
fn value_flows_to_parent(current: Node<'_>, parent: Node<'_>) -> bool {
    let field_is = |field: &str| {
        parent
            .child_by_field_name(field)
            .is_some_and(|child| child.id() == current.id())
    };
    match parent.kind() {
        "parenthesized_expression"
        | "as_expression"
        | "satisfies_expression"
        | "non_null_expression"
        | "await_expression" => true,
        "ternary_expression" => field_is("consequence") || field_is("alternative"),
        "binary_expression" => parent
            .child_by_field_name("operator")
            .is_some_and(|operator| matches!(operator.kind(), "&&" | "||" | "??")),
        "sequence_expression" => {
            let mut cursor = parent.walk();
            parent
                .named_children(&mut cursor)
                .filter(|child| child.kind() != "comment")
                .last()
                .is_some_and(|last| last.id() == current.id())
        }
        _ => false,
    }
}

/// 小文字で始まる JSX の要素名 (`<label>`) か。組み込み要素を指し、同名の関数・変数へは
/// 束縛されない。
fn is_intrinsic_jsx_name(node: Node<'_>, source: &[u8]) -> bool {
    node.kind() == "identifier"
        && node.parent().is_some_and(|parent| {
            matches!(
                parent.kind(),
                "jsx_opening_element" | "jsx_closing_element" | "jsx_self_closing_element"
            ) && parent
                .child_by_field_name("name")
                .is_some_and(|name| name.id() == node.id())
        })
        && node
            .utf8_text(source)
            .ok()
            .and_then(|text| text.chars().next())
            .is_some_and(|first| first.is_ascii_lowercase())
}

/// 識別子 `node` を callee とする直接呼び出し (`f(...)`)。`f?.()`・`new f()`・メンバー経由は除く。
fn direct_call(node: Node<'_>) -> Option<Node<'_>> {
    if node.kind() != "identifier" {
        return None;
    }
    let call = node.parent().filter(|p| p.kind() == "call_expression")?;
    (call
        .child_by_field_name("function")
        .is_some_and(|f| f.id() == node.id())
        && call.child_by_field_name("optional_chain").is_none())
    .then_some(call)
}

/// `function f() {}` / `export function f() {}` がファイル直下にあるか (default export を除く)。
fn is_top_level_declaration(function: Node<'_>) -> bool {
    match function.parent() {
        Some(parent) if parent.kind() == "program" => true,
        Some(parent) if parent.kind() == "export_statement" => {
            !has_token(parent, "default")
                && parent.parent().is_some_and(|root| root.kind() == "program")
        }
        _ => false,
    }
}

/// 値を作る箇所 (関数・変数の宣言・`satisfies` 式) の値を束縛する名前。
enum ProducerName {
    /// これらの名前の `typeof` を確かめる。値を入れた外側のオブジェクト・配列の束縛名も含む
    /// (`const box = { item: x satisfies Row }` の `typeof box` も Row を写す)。`bool` は、
    /// その名前が値を入れた入れ物か (中身の読み出し `box.item` も値を写す)。
    Named(Vec<(String, bool)>),
    /// 名前を持たない (呼び出しの引数・return の値など)。名前で `typeof` されない。
    Anonymous,
    /// default export・クラスのメンバー・分割代入など、名前で利用者を追えない。
    Hidden,
}

fn producer_binding(node: Node<'_>, source: &[u8]) -> ProducerName {
    let text = |name: Node<'_>| match name.kind() {
        "identifier" | "property_identifier" => name.utf8_text(source).ok().map(str::to_string),
        "string" => super::static_js_string_text(name, source).map(str::to_string),
        _ => None,
    };
    let mut names: Vec<(String, bool)> = Vec::new();
    match node.kind() {
        "function_declaration" => {
            if node
                .parent()
                .is_some_and(|p| p.kind() == "export_statement" && has_token(p, "default"))
            {
                return ProducerName::Hidden;
            }
            return match node.child_by_field_name("name").and_then(text) {
                Some(name) => ProducerName::Named(vec![(name, false)]),
                None => ProducerName::Hidden,
            };
        }
        "variable_declarator" => {
            return match node.child_by_field_name("name").and_then(text) {
                Some(name) => ProducerName::Named(vec![(name, false)]),
                None => ProducerName::Hidden,
            };
        }
        "method_definition" => {
            // クラスのメンバーの型は、クラスの型 (`Repo["find"]`・`implements Repo`) を通じて、
            // 名前の参照なしに別の値へ写る。
            if node.parent().is_none_or(|p| p.kind() != "object") {
                return ProducerName::Hidden;
            }
            match node.child_by_field_name("name").and_then(text) {
                Some(name) => names.push((name, false)),
                None => return ProducerName::Hidden,
            }
        }
        _ => {}
    }
    // 式の位置の値は、値がそのまま伝わる式と、値を入れたオブジェクト・配列を上って束縛する
    // 構文を探す。途中のキーの名前も集める。
    let mut current = node;
    let mut contained = false;
    let mut steps = 0;
    loop {
        steps += 1;
        if steps > MAX_UNWRAP {
            // 入れ子が深すぎて束縛を確かめられない。
            return ProducerName::Hidden;
        }
        let Some(parent) = current.parent() else {
            break;
        };
        let field_is = |field: &str| {
            parent
                .child_by_field_name(field)
                .is_some_and(|child| child.id() == current.id())
        };
        if value_flows_to_parent(current, parent) {
            current = parent;
            continue;
        }
        // 名前の無い入れ物から中身を取り出す (`[<値>][0]`・`({ item: <値> }).item`) と、
        // 取り出した値の行き先を名前で追えない。値そのもののメンバーの読み出しは下で扱う。
        if contained && matches!(parent.kind(), "member_expression" | "subscript_expression") {
            return ProducerName::Hidden;
        }
        match parent.kind() {
            "object" | "array" | "spread_element" => {
                contained = true;
                current = parent;
            }
            // キーの値が入れ物なら (`{ inner: { item: <値> } }` の `inner`)、そのキーの名前も
            // 入れ物として確かめる。外側のオブジェクトは次の段で入れ物になる。
            "pair" if field_is("value") => {
                match parent.child_by_field_name("key").and_then(text) {
                    Some(name) => names.push((name, contained)),
                    None => return ProducerName::Hidden,
                }
                current = parent;
            }
            "variable_declarator" if field_is("value") => {
                return match parent.child_by_field_name("name").and_then(text) {
                    Some(name) => {
                        names.push((name, contained));
                        ProducerName::Named(names)
                    }
                    None => ProducerName::Hidden,
                };
            }
            "assignment_expression" if field_is("right") => {
                let left = parent.child_by_field_name("left");
                return match left
                    .filter(|left| left.kind() == "identifier")
                    .and_then(text)
                {
                    Some(name) => {
                        names.push((name, contained));
                        ProducerName::Named(names)
                    }
                    None => ProducerName::Hidden,
                };
            }
            // 引数・return の値・メンバーの読み出し・条件などから先は推論で型が伝わる
            // (名前で観測する範囲の外)。
            "arguments"
            | "return_statement"
            | "arrow_function"
            | "expression_statement"
            | "jsx_expression"
            | "throw_statement"
            | "template_substitution"
            | "member_expression"
            | "subscript_expression"
            | "call_expression"
            | "new_expression"
            | "unary_expression"
            | "binary_expression"
            | "ternary_expression"
            | "sequence_expression" => break,
            // クラスのフィールドはクラスの型を通じて写り、`export default <値>` は任意の名前で
            // import される。そのほかの知らない構文も、値を別の名前へ渡すかもしれない。
            _ => return ProducerName::Hidden,
        }
    }
    if names.is_empty() {
        ProducerName::Anonymous
    } else {
        ProducerName::Named(names)
    }
}

/// 関数本体の return 文 (入れ子の関数・クラスの中は除く)。
fn function_returns(body: Node<'_>) -> Vec<Node<'_>> {
    let mut returns = Vec::new();
    let _ = walk_preorder(body, Children::Named, |node, depth| {
        if depth > 0
            && matches!(
                node.kind(),
                "function_declaration"
                    | "function_expression"
                    | "function"
                    | "generator_function_declaration"
                    | "generator_function"
                    | "arrow_function"
                    | "method_definition"
                    | "class_declaration"
                    | "abstract_class_declaration"
                    | "class"
            )
        {
            return ControlFlow::<(), Walk>::Continue(Walk::Skip);
        }
        if node.kind() == "return_statement" {
            returns.push(node);
        }
        ControlFlow::Continue(Walk::Descend)
    });
    returns
}

/// 本体が引数 `name` へ書き込むか、`var` で束縛し直すか、`typeof name` で型を取り出すか。
/// 入れ子の関数の中も見る (クロージャからの再代入もある)。
fn body_rebinds_or_queries(body: Node<'_>, source: &[u8], name: &str) -> bool {
    any_node(body, Children::Named, |node| match node.kind() {
        "assignment_expression" | "augmented_assignment_expression" => node
            .child_by_field_name("left")
            .is_some_and(|left| assignment_target_binds(left, source, name)),
        "update_expression" => node
            .child_by_field_name("argument")
            .is_some_and(|argument| assignment_target_binds(argument, source, name)),
        // `for (row of rows)` は既存の束縛への代入、`for (var row of rows)` は引数と同じ束縛。
        "for_in_statement" => {
            node.child_by_field_name("kind")
                .is_none_or(|kind| kind.kind() == "var")
                && node
                    .child_by_field_name("left")
                    .is_some_and(|left| assignment_target_binds(left, source, name))
        }
        // `var row = ...` は引数と同じ束縛へ代入する。
        "variable_declarator" => {
            node.parent()
                .is_some_and(|p| p.kind() == "variable_declaration")
                && node
                    .child_by_field_name("name")
                    .is_some_and(|pattern| pattern_binds_name(pattern, source, name))
        }
        "type_query" => any_node(node, Children::Named, |n| {
            n.kind() == "identifier" && n.utf8_text(source).ok() == Some(name)
        }),
        // 引数の値を別の名前へ束縛する・入れ物に入れる (`const alias = row` の先の
        // `typeof alias` は引数の名前の参照に現れない)。メンバーの読み出し・呼び出しの引数は
        // 引数の値を写さない。
        "identifier" | "shorthand_property_identifier" => {
            node.utf8_text(source).ok() == Some(name)
                && !is_binding_name(node)
                && binds_value_to_another_name(node, source, false)
        }
        _ => false,
    })
}

/// 束縛を宣言する名前の位置か (引数・変数・catch・ループ変数・分割代入の束縛)。値の読み出し
/// ではない。
fn is_binding_name(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    let field_is = |field: &str| {
        parent
            .child_by_field_name(field)
            .is_some_and(|child| child.id() == node.id())
    };
    match parent.kind() {
        "required_parameter" | "optional_parameter" => field_is("pattern"),
        "variable_declarator" => field_is("name"),
        "arrow_function" | "catch_clause" => field_is("parameter"),
        "for_in_statement" => field_is("left"),
        "assignment_pattern" | "object_assignment_pattern" => field_is("left"),
        "pair_pattern" => field_is("value"),
        "formal_parameters" | "array_pattern" | "object_pattern" | "rest_pattern" => true,
        "function_declaration" | "function_expression" | "class_declaration" | "class" => {
            field_is("name")
        }
        _ => false,
    }
}

/// 代入の左辺が `name` そのもの (または分割代入の束縛) か。プロパティへの代入は含めない。
fn assignment_target_binds(target: Node<'_>, source: &[u8], name: &str) -> bool {
    let mut current = target;
    for _ in 0..MAX_UNWRAP {
        match current.kind() {
            "identifier" => return current.utf8_text(source).ok() == Some(name),
            "parenthesized_expression" => match only_named_child(current) {
                Some(inner) => current = inner,
                None => return true,
            },
            "object_pattern" | "array_pattern" => return pattern_binds_name(current, source, name),
            _ => return false,
        }
    }
    true
}

/// 関数系の scope の引数のうち、単純な識別子で `name` を束縛するもの。
fn parameter_named<'t>(scope: Node<'t>, source: &[u8], name: &str) -> Option<Node<'t>> {
    let params = scope.child_by_field_name("parameters")?;
    let mut cursor = params.walk();
    params.named_children(&mut cursor).find(|param| {
        matches!(param.kind(), "required_parameter" | "optional_parameter")
            && param
                .child_by_field_name("pattern")
                .is_some_and(|p| p.kind() == "identifier" && p.utf8_text(source).ok() == Some(name))
    })
}

/// import 文が `name` を別名なしの named import で束縛するか。値として使う import は
/// 指定子の `type` 修飾も受け付けない。
fn named_import_specifier(
    statement: Node<'_>,
    source: &[u8],
    name: &str,
    import_use: ImportUse,
) -> bool {
    any_node(statement, Children::Named, |node| {
        node.kind() == "import_specifier"
            && node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
                == Some(name)
            && node
                .child_by_field_name("alias")
                .is_none_or(|alias| alias.utf8_text(source).ok() == Some(name))
            && (import_use == ImportUse::Type || !has_token(node, "type"))
    })
}

/// 冗長な括弧だけを剥がした式。
fn unwrap_parens(expr: Node<'_>) -> Option<Node<'_>> {
    let mut current = expr;
    for _ in 0..MAX_UNWRAP {
        if current.kind() != "parenthesized_expression" {
            return Some(current);
        }
        current = first_named_child(current)?;
    }
    None
}

/// プロパティの値・初期化子として型を読める式。括弧と、primitive の literal への `as const`
/// (`"ready" as const`。literal 型のまま) だけを剥がす。`satisfies T` は中の literal の
/// 文脈の型を変え (`["ready"] satisfies string[]` は `string[]`)、配列への `as const` は
/// readonly な tuple にするので、どちらも型を読まない。
fn unwrap_literal_value(expr: Node<'_>) -> Option<Node<'_>> {
    let value = unwrap_parens(expr)?;
    match value.kind() {
        "as_expression" if is_as_const(value) => {
            let inner = unwrap_parens(first_named_child(value)?)?;
            is_primitive_literal(inner).then_some(inner)
        }
        "as_expression" | "satisfies_expression" => None,
        _ => Some(value),
    }
}

/// 値 `undefined` の型。ファイルのどこかで `undefined` という名前を束縛し直していれば
/// (`function f(undefined: string)`)、組み込みの値と決められないので None。
fn undefined_type(parsed: &ParsedRefFile) -> Option<Vec<TypeAtom>> {
    let source: &[u8] = &parsed.source;
    let rebound = any_node(parsed.tree.root_node(), Children::Named, |node| {
        let named_undefined = matches!(
            node.kind(),
            "undefined" | "identifier" | "shorthand_property_identifier_pattern"
        ) && node.utf8_text(source).ok() == Some("undefined");
        named_undefined
            && node.parent().is_some_and(|parent| {
                let field_is = |field: &str| {
                    parent
                        .child_by_field_name(field)
                        .is_some_and(|child| child.id() == node.id())
                };
                match parent.kind() {
                    "required_parameter" | "optional_parameter" => field_is("pattern"),
                    "variable_declarator"
                    | "function_declaration"
                    | "function_expression"
                    | "class_declaration"
                    | "class" => field_is("name"),
                    "assignment_expression"
                    | "augmented_assignment_expression"
                    | "assignment_pattern"
                    | "object_assignment_pattern"
                    | "for_in_statement" => field_is("left"),
                    "pair_pattern" => field_is("value"),
                    "arrow_function" | "catch_clause" => field_is("parameter"),
                    "update_expression" => field_is("argument"),
                    "formal_parameters" | "array_pattern" | "object_pattern" | "rest_pattern"
                    | "import_specifier" | "namespace_import" | "import_clause" => true,
                    _ => false,
                }
            })
    });
    (!rebound).then(|| vec![TypeAtom::Undefined])
}

/// 文字列・数値・真偽値・null・undefined の literal と、置換を含まない template literal。
fn is_primitive_literal(node: Node<'_>) -> bool {
    match node.kind() {
        "string" | "number" | "true" | "false" | "null" | "undefined" => true,
        "unary_expression" => node
            .child_by_field_name("argument")
            .is_some_and(|argument| argument.kind() == "number"),
        "template_string" => {
            let mut cursor = node.walk();
            !node
                .named_children(&mut cursor)
                .any(|child| child.kind() == "template_substitution")
        }
        _ => false,
    }
}

/// `expr as const` か (型を書いた `as T` ではない)。
fn is_as_const(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    let children: Vec<Node<'_>> = node
        .children(&mut cursor)
        .filter(|c| c.kind() != "comment")
        .collect();
    matches!(children.as_slice(), [_, as_token, const_token]
        if as_token.kind() == "as" && const_token.kind() == "const")
}

/// コメントを除いた最初の名前付きの子。
fn first_named_child(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| child.kind() != "comment")
}

/// コメントを除いた名前付きの子がちょうど 1 つなら、その子。
fn only_named_child(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    let mut children = node
        .named_children(&mut cursor)
        .filter(|child| child.kind() != "comment");
    let only = children.next()?;
    children.next().is_none().then_some(only)
}

/// 無名のトークン `token` を直接の子に持つか。
fn has_token(node: Node<'_>, token: &str) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| !child.is_named() && child.kind() == token)
}

/// 名前の参照をまだ集めていなければ、まとめて 1 回の検索で集める。
fn ensure_name_refs(dir: &str, names: &BTreeSet<&str>, caches: &mut ApiClosureCaches) {
    let missing: Vec<String> = names
        .iter()
        .filter(|name| !caches.ts_name_refs.contains_key(**name))
        .map(|name| name.to_string())
        .collect();
    if missing.is_empty() {
        return;
    }
    match crate::service::AppService::new().find_symbol_references_batch(
        &missing,
        dir,
        Some(NAME_REFS_GLOB),
    ) {
        Ok(results) => {
            let mut by_name: HashMap<String, crate::models::reference::RefsResult> = results
                .into_iter()
                .map(|result| (result.symbol.clone(), result))
                .collect();
            for name in missing {
                // 読めない・解析できないファイルがあれば、その中の参照は見えていない。
                let refs = by_name
                    .remove(&name)
                    .filter(|result| result.failed_files == 0)
                    .map(|result| Rc::new(result.references));
                caches.ts_name_refs.insert(name, refs);
            }
        }
        Err(_) => {
            for name in missing {
                caches.ts_name_refs.insert(name, None);
            }
        }
    }
}

fn name_refs(caches: &ApiClosureCaches, name: &str) -> Option<Rc<Vec<SymbolReference>>> {
    caches.ts_name_refs.get(name).cloned().flatten()
}

/// 関係する名前が、名前の参照 (識別子) に現れない書き方で使われ得るか。読めない・解析できない
/// ファイルと、ファイルの一覧を取れない場合は、使われ得るとみなす。
/// - JS 系のファイル (`.js` / `.jsx` / `.mjs` / `.cjs`) の JSDoc に `comment_names` が識別子の
///   単語として現れる。checkJs では JSDoc も型として検査されるが、コメントは参照に現れない
/// - `string_names` と同じ文字列で、名前空間オブジェクトや入れ物から値・型を取り出す
///   (`rows["toRow"]`・`const { "item": picked } = box`・`(typeof rows)["toRow"]`)
fn sources_may_mention_outside_identifiers(
    dir: &str,
    comment_names: &[&str],
    string_names: &[&str],
    caches: &mut ApiClosureCaches,
) -> bool {
    let files = caches
        .ts_checked_files
        .get_or_insert_with(|| {
            crate::engine::refs::collect_files_scan(
                Path::new(dir),
                Some(NAME_REFS_GLOB),
                crate::engine::refs::FileScanOptions::default(),
            )
            .ok()
            .map(|collection| Rc::new(collection.files))
        })
        .clone();
    let Some(files) = files else {
        return true;
    };
    let mentions = |text: &str, names: &[&str]| {
        names
            .iter()
            .any(|name| super::ref_index::contains_js_word(text, name))
    };
    // 型検査の対象をすべて読むので、ファイルごとに並列に調べる。
    files.par_iter().any(|path| {
        let Some(lang) = path
            .to_str()
            .and_then(|path| LangId::from_path(camino::Utf8Path::new(path)).ok())
        else {
            return true;
        };
        let Some(text) =
            crate::engine::bounded_read::read_utf8_file_limited(path, MAX_SOURCE_SCAN_BYTES)
        else {
            return true;
        };
        // tsc が型として読むコメントは、JS 系のファイルの JSDoc だけ (TS のファイルの JSDoc は
        // 型に使わない)。
        let check_comments = lang == LangId::Javascript && mentions(&text, comment_names);
        if !check_comments && !mentions(&text, string_names) {
            return false;
        }
        let Ok(tree) = crate::engine::parser::parse_source(text.as_bytes(), lang) else {
            return true;
        };
        let source = text.as_bytes();
        any_node(tree.root_node(), Children::Named, |node| {
            match node.kind() {
                // `/** ... */` の型は `{...}` で囲まなくても読まれる (`@type ReturnType<typeof toRow>`)。
                // 普通のコメントは見ない。
                "comment" => {
                    check_comments
                        && node.utf8_text(source).is_ok_and(|comment| {
                            comment.starts_with("/**") && mentions(comment, comment_names)
                        })
                }
                "string" | "template_string" | "template_literal_type" => {
                    is_string_access_key(node) && string_may_equal(node, source, string_names)
                }
                _ => false,
            }
        })
    })
}

/// 文字列が、名前で値や型を取り出す位置にあるか。値の添字 (`rows["toRow"]`)、分割代入の
/// キー (`const { "item": picked } = box`)、型の添字 (`(typeof rows)["toRow"]`)。値を変えずに
/// 包む式 (`rows["toRow" as const]`・`rows[flag ? "toRow" : "x"]`)・型アサーション・computed
/// key (`const { ["toRow"]: make } = rows`)・型の union と括弧 (`(typeof rows)["a" | "b"]`) は
/// 上ってから位置を見る。
fn is_string_access_key(node: Node<'_>) -> bool {
    let mut current = node;
    for _ in 0..MAX_UNWRAP {
        let Some(parent) = current.parent() else {
            return false;
        };
        let field_is = |field: &str| {
            parent
                .child_by_field_name(field)
                .is_some_and(|child| child.id() == current.id())
        };
        if value_flows_to_parent(current, parent) {
            current = parent;
            continue;
        }
        match parent.kind() {
            "type_assertion"
            | "computed_property_name"
            | "literal_type"
            | "union_type"
            | "parenthesized_type" => current = parent,
            "subscript_expression" => return field_is("index"),
            "pair_pattern" => return field_is("key"),
            "lookup_type" => return first_named_child(parent) != Some(current),
            _ => return false,
        }
    }
    false
}

/// 置換の無い文字列の中身が、`names` のどれかと同じ名前になり得るか。escape を含む文字列は
/// 名前を比べられないので、同じ名前になり得るとみなす。
fn string_may_equal(node: Node<'_>, source: &[u8], names: &[&str]) -> bool {
    if matches!(node.kind(), "template_string" | "template_literal_type") {
        let mut cursor = node.walk();
        if node
            .named_children(&mut cursor)
            .any(|child| matches!(child.kind(), "template_substitution" | "template_type"))
        {
            return false;
        }
    }
    super::static_js_string_text(node, source)
        .is_some_and(|text| text.contains('\\') || names.contains(&text))
}

/// 相対 import の解決を変える設定 (`rootDirs` / `moduleSuffixes`) が、参照元から解析 root
/// までの tsconfig / jsconfig に書かれていないか。設定ファイルを読めなければ false。
fn relative_resolution_is_plain(dir: &str, from_path: &str, caches: &mut ApiClosureCaches) -> bool {
    let mut current = Path::new(from_path).parent();
    loop {
        let relative = current
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let plain = *caches
            .ts_plain_resolution_dirs
            .entry(relative.clone())
            .or_insert_with(|| directory_configs_are_plain(dir, &relative));
        if !plain {
            return false;
        }
        match current {
            Some(path) if !path.as_os_str().is_empty() => current = path.parent(),
            _ => return true,
        }
    }
}

fn directory_configs_are_plain(dir: &str, relative: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(Path::new(dir).join(relative)) else {
        return false;
    };
    for entry in entries {
        let Ok(entry) = entry else {
            return false;
        };
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        if !(file_name.starts_with("tsconfig") || file_name.starts_with("jsconfig"))
            || !file_name.ends_with(".json")
        {
            continue;
        }
        match crate::engine::bounded_read::read_utf8_file_limited(&entry.path(), MAX_CONFIG_BYTES) {
            Some(text) if !text.contains("rootDirs") && !text.contains("moduleSuffixes") => {}
            _ => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> tree_sitter::Tree {
        crate::engine::parser::parse_source(source.as_bytes(), LangId::Typescript).unwrap()
    }

    /// `type T = <ty>;` の右辺を単純な型として読む。
    fn type_of(ty: &str) -> Option<Vec<TypeAtom>> {
        let source = format!("type T = {ty};\n");
        let tree = parse(&source);
        let alias = tree.root_node().named_child(0).unwrap();
        simple_type(
            alias.child_by_field_name("value").unwrap(),
            source.as_bytes(),
        )
    }

    #[test]
    fn simple_type_reads_predefined_literal_union_and_array_types() {
        use TypeAtom::*;
        assert_eq!(type_of("string | null"), Some(vec![String, Null]));
        assert_eq!(
            type_of("\"a\" | 'b' | -1 | 2 | true | undefined"),
            Some(vec![
                Undefined,
                StrLit(Some("a".into())),
                StrLit(Some("b".into())),
                NumLit("-1".into()),
                NumLit("2".into()),
                True,
            ])
        );
        assert_eq!(
            type_of("(string | number)[]"),
            Some(vec![Array(vec![String, Number])])
        );
        assert_eq!(type_of("unknown"), Some(vec![Unknown]));
        // 型の参照・generic・関数型・escape を含む literal の比較は扱わない。
        assert_eq!(type_of("Note"), None);
        assert_eq!(type_of("Array<string>"), None);
        assert_eq!(type_of("() => void"), None);
        assert_eq!(type_of("\"a\\\"b\""), Some(vec![StrLit(None)]));
    }

    #[test]
    fn simple_type_handles_long_unions_without_recursion() {
        let members: Vec<String> = (0..20_000).map(|i| format!("\"v{i}\"")).collect();
        let atoms = type_of(&members.join(" | ")).expect("long union");
        assert_eq!(atoms.len(), 20_000);
    }

    #[test]
    fn assignable_follows_literal_and_union_rules() {
        use TypeAtom::*;
        let note = vec![String, Null];
        assert!(assignable(&[Null], &note));
        assert!(assignable(&[StrLit(Some("x".into()))], &note));
        assert!(!assignable(&[NumLit("123".into())], &note));
        assert!(!assignable(&[Undefined], &note));
        let state = vec![StrLit(Some("ready".into()))];
        assert!(assignable(&[StrLit(Some("ready".into()))], &state));
        assert!(!assignable(&[String], &state));
        assert!(!assignable(&[StrLit(None)], &state));
        assert!(assignable(&[Boolean], &[True, False]));
        assert!(!assignable(&[Boolean], &[True]));
        assert!(assignable(&[Array(vec![])], &[Array(vec![String])]));
        assert!(!assignable(&[Array(vec![Number])], &[Array(vec![String])]));
        assert!(assignable(&[Number], &[Unknown]));
        assert!(assignable(&[Any], &[String]));
    }

    #[test]
    fn literal_values_unwrap_only_parentheses_and_primitive_const_assertions() {
        let source = "const a = (\"ready\" as const);\nconst b = [\"ready\"] as const;\nconst c = [\"ready\"] satisfies string[];\nconst d = (((1)));\nconst e = { id: 1 } as Row;\n";
        let tree = parse(source);
        let values: Vec<_> = (0..5)
            .map(|i| {
                let declaration = tree.root_node().named_child(i).unwrap();
                let declarator = declaration.named_child(0).unwrap();
                unwrap_literal_value(declarator.child_by_field_name("value").unwrap())
                    .map(|node| node.kind())
            })
            .collect();
        assert_eq!(values, [Some("string"), None, None, Some("number"), None]);
    }

    #[test]
    fn bigint_literals_are_distinct_types() {
        use TypeAtom::*;
        assert_eq!(type_of("1n"), Some(vec![BigIntLit("1n".into())]));
        assert!(!assignable(
            &[BigIntLit("2n".into())],
            &[BigIntLit("1n".into())]
        ));
        assert!(assignable(&[BigIntLit("2n".into())], &[BigInt]));
        assert!(!assignable(&[BigInt], &[BigIntLit("1n".into())]));
    }

    #[test]
    fn body_rebinding_detection_ignores_property_writes() {
        let cases = [
            ("row = other;", true),
            ("({ row } = other);", true),
            ("row++;", true),
            ("for (row of rows) {}", true),
            ("for (var row of rows) {}", true),
            ("var row = other;", true),
            ("const copy: typeof row = other;", true),
            ("const other = row;", true),
            ("const copy = { ...row };", true),
            ("row.note = null;", false),
            ("for (const row of rows) {}", false),
            ("const id = row.id;", false),
            ("const { id } = row;", false),
            ("label(row);", false),
        ];
        for (body, expected) in cases {
            let source = format!("function f(row: Row) {{ {body} }}\n");
            let tree = parse(&source);
            let function = tree.root_node().named_child(0).unwrap();
            let body_node = function.child_by_field_name("body").unwrap();
            assert_eq!(
                body_rebinds_or_queries(body_node, source.as_bytes(), "row"),
                expected,
                "{body}"
            );
        }
    }

    #[test]
    fn type_role_requires_exact_annotation_positions() {
        let source = "function a(): Row { return x; }\nconst b: Row = x;\nconst c = x satisfies Row;\nfunction d(row: Row) {}\nfunction e(row?: Row) {}\nconst f: Row[] = [];\nfunction g(): Row | null { return null; }\n";
        let tree = parse(source);
        let mut roles = Vec::new();
        let _ = walk_preorder(tree.root_node(), Children::Named, |node, _| {
            if node.kind() == "type_identifier"
                && node.utf8_text(source.as_bytes()).ok() == Some("Row")
            {
                roles.push(match type_role(node) {
                    Some(TypeRole::Return(_)) => "return",
                    Some(TypeRole::Variable(_)) => "variable",
                    Some(TypeRole::Satisfies(_)) => "satisfies",
                    Some(TypeRole::Parameter(_)) => "parameter",
                    None => "none",
                });
            }
            ControlFlow::<(), Walk>::Continue(Walk::Descend)
        });
        assert_eq!(
            roles,
            [
                "return",
                "variable",
                "satisfies",
                "parameter",
                "none",
                "none",
                "none"
            ]
        );
    }
}
