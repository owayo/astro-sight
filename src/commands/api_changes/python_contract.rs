//! Python 固有の型契約変更の分類 (TypedDict の `total=` 変更)。
//!
//! `api.mod` は「シグネチャが変わった」までしか言えないため、トリアージは毎回ソースを開いて
//! 「引数が増えたのか / 型が狭まったのか / キーが必須化したのか」を読み直していた。ここでは
//! `class X(TypedDict, total=False)` ↔ `class X(TypedDict)` を種別として確定し、
//! 「壊れるのは値を作る側か読む側か」を出力に添える (Issue
//! 2026-08-18-python-typeddict-contract-change-classification)。
//!
//! **severity は一切変えない**。分類できたものは `modified` (blocking) に残り、分類できなければ
//! `None` を返して従来どおり素の `api.mod` になる。`total=False` の除去は「値を作る側が壊れる」
//! 方向なので、リポジトリ内の参照が同一 diff で更新済みでも非 blocking へ落としてはならない
//! (外部リポジトリ / 動的生成された dict は静的に追えない)。呼び出し側の
//! `classify_signature_change` が、分類できた変更を降格経路より前で `modified` に確定させる。
//!
//! 判定は fail-closed。証明できない形 (未知の import 元 / 未知の基底クラス /
//! `total=FLAG` のような非リテラル / `**options` / パースエラー / `TypedDict` 名の shadow /
//! `globals()` 等の動的 namespace 操作) は `None` を返し、素の `api.mod` に残す。
//!
//! **保証の範囲 (正直に書く)**: 「静的に宣言された形からしか分類しない」までで、
//! Python の reflection 一般に対する完全性は保証しない (`exec` で組み立てた名前、
//! 別モジュールで `X = NotRequired[str]` と定義された型エイリアスの import 等)。
//! 保証を限定できる理由は、**分類が誤っても severity が変わらない**こと — 対象は必ず
//! blocking な `api.mod` に残るので、最悪ケースは「ラベルが実態とずれてトリアージの
//! 初手が逆になる」であって、破壊的変更の見逃しにはならない。降格判定 (fail-open が
//! 破壊的変更の見逃しに直結する側) には一切関与しない。
//!
//! **対象外 (別 Issue)**: `NotRequired[...]` の追加・削除と `Literal` の値集合変更。どちらも
//! 現状そもそも検出されておらず (クラスヘッダ行が変わらない / 型エイリアスが公開シンボルでない)、
//! 分類ではなく新規検出の追加になるため、検出面・公開判定・出力単位を別途設計する。

use std::collections::{BTreeMap, HashSet};

use crate::language::LangId;
use crate::models::review::{ApiContractChange, ApiContractChangeKind, ApiContractSide};

use super::source_pair::{CompatibleModSite, SignatureSourceCache};

/// `typing` / `typing_extensions` から来た `TypedDict` だけを本物として認める。
const TYPED_DICT_MODULES: [&str; 2] = ["typing", "typing_extensions"];

/// TypedDict クラス 1 件から読み取った契約事実。
#[derive(Debug, PartialEq, Eq)]
struct TypedDictFact {
    /// 実効 total。省略 / `total=True` は `true`、`total=False` は `false`。
    effective_total: bool,
    /// 基底クラス名 (宣言順)。old/new で一致しなければ継承フィールドが増減しうるため分類しない。
    bases: Vec<String>,
    /// PEP 695 の型パラメータ (`class B[T](TypedDict)`) のテキスト。無ければ `None`。
    /// 基底と同じ理由で old/new の一致を要求する。
    type_params: Option<String>,
    /// このクラス自身が宣言したフィールドの `名前 → 型注釈テキスト`。
    /// `total` は自クラスで宣言したフィールドにしか作用しないため、継承分は含めない。
    own_fields: BTreeMap<String, String>,
}

/// Python の TypedDict で `total=` の実効値が反転した api.mod を型契約変更として分類する。
///
/// 次をすべて満たすときだけ `Some` を返す:
/// - Python の `class` シンボルで、old/new とも module 直下に同名クラスが一意に存在する
/// - 基底クラスがすべて `typing` / `typing_extensions` の `TypedDict`、または同一ファイル内で
///   同様に証明できた TypedDict のサブクラス
/// - クラス引数の keyword が `total` のみ、値が `True` / `False` リテラル、重複なし
/// - 基底クラス列・型パラメータ・own フィールドの集合と型注釈が old/new で完全に一致する
///   (total 以外の変更が混ざると「壊れる側」が一意に決まらない)
/// - own フィールドのうち少なくとも 1 件が「requiredness 修飾子ではありえない」と証明できる
///   (`a: int` 等)。これが無いと `total` を反転しても実効 requiredness が変わる保証が無い
/// - 実効 total が `false` ↔ `true` で反転している (省略 ↔ `total=True` は意味的に同値なので対象外)
pub(crate) fn detect_python_typed_dict_total_change(
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache,
) -> PythonContractDetection {
    use PythonContractDetection as D;

    let Some(lang) = site.lang_in(&[LangId::Python]) else {
        return D::NotApplicable;
    };
    if site.kind != "class" {
        return D::NotApplicable;
    }
    // 実効 total が反転するなら、どちらかのヘッダに必ず `total` の字面がある。
    // ここで弾いておくと、無関係な Python class の api.mod で `git show` + 両側 parse が
    // 走らない (この判定器が SignatureSourceCache を最初に触る位置にあるため)。
    if !site.old_sig.contains("total") && !site.new_sig.contains("total") {
        return D::NotApplicable;
    }

    // ここから先は「クラスヘッダに `total` が現れる Python class の api.mod」。
    // **ここで `NotApplicable` を返すと降格経路に落ちる**ので、以降の失敗はすべて
    // `PotentialBreakingChange` (ラベルは付けないが降格もさせない) に倒す。
    // 分類できないこと自体は「破壊的でない」証明にはならない — 例えば `NotRequired` が
    // 同居するファイルは分類対象外だが、bare フィールドの requiredness は実際に反転する。

    // qualname 形式 (`Outer.Inner`) は module 直下のクラスではないので解析対象にできないが、
    // 入れ子の TypedDict である可能性は否定できないため降格もさせない。
    if site.name.contains('.') {
        return D::PotentialBreakingChange;
    }
    let Some(src) = sources.get(site) else {
        return D::PotentialBreakingChange;
    };
    let Some((old_tree, new_tree)) = src.parse_pair(lang) else {
        return D::PotentialBreakingChange;
    };
    let (Some(old_fact), Some(new_fact)) = (
        analyze_typed_dict(old_tree.root_node(), &src.old, site.name),
        analyze_typed_dict(new_tree.root_node(), &src.new, site.name),
    ) else {
        return D::PotentialBreakingChange;
    };

    // total 以外の変更が混ざっていると「壊れる側」が一意に決まらないため種別は付けない。
    // 基底クラスが変わると継承フィールドが増減し、型パラメータが変わると具体化が変わる。
    if old_fact.own_fields != new_fact.own_fields
        || old_fact.bases != new_fact.bases
        || old_fact.type_params != new_fact.type_params
    {
        return D::PotentialBreakingChange;
    }
    match (old_fact.effective_total, new_fact.effective_total) {
        // 省略可だったキーが必須化する → 値を作る側 (dict literal を組む呼び出し側) が壊れる。
        (false, true) => D::Classified(ApiContractChange {
            kind: ApiContractChangeKind::TypedDictTotalFalseRemoved,
            breaks: ApiContractSide::Producer,
        }),
        // 必須だったキーが省略可になる → 値を読む側が壊れる。
        (true, false) => D::Classified(ApiContractChange {
            kind: ApiContractChangeKind::TypedDictTotalFalseAdded,
            breaks: ApiContractSide::Consumer,
        }),
        // 実効 total が変わらないと**証明できた**ケースだけ、従来どおり降格判定へ戻す
        // (省略 ↔ `total=True` は意味的に同値)。
        _ => D::NotApplicable,
    }
}

/// `total=` 変更の判定結果。**ラベル分類と severity ガードを分ける**ための 3 値。
///
/// 2 値 (`Option`) にすると「分類できなかった」が「降格してよい」と同義になり、
/// `NotRequired` が同居して分類を諦めたケースが `modified_closed_in_diff` へ降格して
/// 破壊的変更を見逃す (レビュー 3 巡目の指摘)。分類の成否と blocking の維持は独立させる。
#[derive(Debug)]
pub(crate) enum PythonContractDetection {
    /// TypedDict の `total=` 変更ではない。従来どおり降格判定へ進んでよい。
    NotApplicable,
    /// `total=` 変更の可能性があるが種別を確定できない。ラベルは付けず、降格もさせない。
    PotentialBreakingChange,
    /// 種別まで確定した。ラベルを付けて、降格もさせない。
    Classified(ApiContractChange),
}

/// ソースを読まずに「Python の TypedDict `total=` 変更候補か」を判定する安価なゲート。
///
/// `detect_python_typed_dict_total_change` の前半 3 条件と同じ基準。api.mod 候補を
/// 早期に捨てる経路 (同一ファイル内でしか使われていないシンボルの除外) が、契約変更の
/// 判定より前にあるため、そこで落としてよいかをここで見分ける。
pub(crate) fn may_be_python_typed_dict_total_change(
    kind: &str,
    old_sig: &str,
    new_sig: &str,
    lang: Option<LangId>,
) -> bool {
    lang == Some(LangId::Python)
        && kind == "class"
        && (old_sig.contains("total") || new_sig.contains("total"))
}

/// 1 ソースから対象クラスの契約事実を取り出す。証明できない形はすべて `None`。
fn analyze_typed_dict(
    root: tree_sitter::Node<'_>,
    source: &[u8],
    class_name: &str,
) -> Option<TypedDictFact> {
    if root.has_error() {
        return None;
    }
    // `Required` / `NotRequired` が現れるファイルは対象外。これらはフィールド単位で
    // `total` を打ち消すため、alias (`from typing import NotRequired as NR`) や別ファイルの
    // 型エイリアス経由まで含めて「実効 requiredness が変わったか」を証明する必要がある。
    // 別 Issue で検出面ごと設計するまで、ファイル全体を保守的に諦める。
    if source_mentions_requiredness_qualifier(source) {
        return None;
    }
    let typed_dict_names = collect_typed_dict_names(root, source)?;
    if typed_dict_names.is_empty() {
        return None;
    }

    // 認識した名前 (`TypedDict` / alias / `import typing as t` の `t`) が、canonical な import
    // 以外でも束縛されていたら、その名前が本当に何を指すか追えない。代入だけでなく
    // `class TypedDict:` / `def TypedDict():` / 二重 import / `del` も shadow になる。
    let bindings = collect_binding_counts(root, source);
    // `globals()[...] = x` / `setattr` / `exec` を使うファイルは、静的に読んだ束縛が
    // 実行時にどうなるか保証できないため一切分類しない。
    if bindings.dynamic_namespace_op {
        return None;
    }
    if !typed_dict_names.iter().all(|n| n.is_provable(&bindings)) {
        return None;
    }

    let classes = module_level_classes(root)?;
    let target = unique_class_named(&classes, source, class_name)?;
    // 基底クラスを名前で照合するため、module 直下のクラス名も一意でなければならない。
    for class in &classes {
        let name = class
            .child_by_field_name("name")
            .and_then(|n| node_text(n, source))?;
        if bindings.count(&name) != 1 {
            return None;
        }
    }

    let proven = prove_typed_dict_classes(&classes, source, &typed_dict_names);
    if !proven.contains(&target.id()) {
        return None;
    }

    let header = class_header(target, source)?;
    let own_fields = class_own_fields(target, source)?;
    // フィールドが 0 件だと total を変えても実効 requiredness は変わらない。
    // さらに「requiredness 修飾子ではありえない」と証明できるフィールドが最低 1 件必要
    // (別モジュールの型エイリアスが `NotRequired[...]` の別名である可能性を排除できないため)。
    if !own_fields
        .values()
        .any(|ty| annotation_is_provably_plain(ty, &bindings))
    {
        return None;
    }
    Some(TypedDictFact {
        effective_total: header.total,
        bases: header.bases,
        type_params: header.type_params,
        own_fields,
    })
}

/// 型注釈が「requiredness 修飾子ではありえない」と証明できるか。
///
/// 証明できるのは、注釈の根が組み込み型 (`int` / `list[str]` 等) で、かつその名前が
/// ファイル内で一切束縛されていない (= 組み込みのまま) 場合だけ。import された名前や
/// module 直下で代入された名前は、別モジュールで `X = NotRequired[str]` と定義されている
/// 可能性を静的に排除できないため証明に使わない。
fn annotation_is_provably_plain(annotation: &str, bindings: &BindingCounts) -> bool {
    /// `total` の影響を必ず受けると言い切れる組み込み型。
    /// `typing` 由来の名前 (`Any` / `Optional` 等) は import された名前なので含めない。
    const PLAIN_ROOTS: [&str; 13] = [
        "int",
        "str",
        "float",
        "bool",
        "bytes",
        "complex",
        "list",
        "dict",
        "set",
        "tuple",
        "frozenset",
        "bytearray",
        "object",
    ];
    let root = annotation
        .split(['[', '.', ' ', '|', ','])
        .next()
        .unwrap_or_default();
    // 添字より後ろ (`list[NotRequired[int]]` 等) は requiredness 修飾子として意味を持たない
    // 位置なので、根だけ見れば足りる。
    if !PLAIN_ROOTS.contains(&root) {
        return false;
    }
    // 組み込み名がファイル内で束縛し直されていたら組み込みではない。
    bindings.count(root) == 0
}

/// `Required` / `NotRequired` の出現を素朴に見る。`NotRequired` は `Required` を含むため
/// 1 パターンで足りる。フィールド名に `Required` を含むだけでも諦めるが、過剰に保守的な側。
fn source_mentions_requiredness_qualifier(source: &[u8]) -> bool {
    source.windows(b"Required".len()).any(|w| w == b"Required")
}

/// module 直下の import から「TypedDict を指す名前」を集める。
///
/// 認識する形:
/// - `from typing import TypedDict` / `from typing_extensions import TypedDict` → `TypedDict`
/// - `from typing import TypedDict as TD` → `TD`
/// - `import typing` → `typing.TypedDict` / `import typing as t` → `t.TypedDict`
///
/// 次は `None` (ファイルごと諦める):
/// - `typing` / `typing_extensions` からの star import (何が入るか読めない)
/// - 未知のモジュールからの star import (`TypedDict` が入りうる)
/// - 未知のモジュールから `TypedDict` という名前を import している (出所を証明できない)
///
/// 条件分岐や try の中の import は module 直下ではないので集めない。結果として基底クラスを
/// 証明できず `None` に倒れる (fail-closed)。
fn collect_typed_dict_names(
    root: tree_sitter::Node<'_>,
    source: &[u8],
) -> Option<HashSet<TypedDictName>> {
    let mut names = HashSet::new();
    let mut cursor = root.walk();
    for stmt in root.named_children(&mut cursor) {
        match stmt.kind() {
            "import_from_statement" => {
                let module = stmt
                    .child_by_field_name("module_name")
                    .and_then(|n| node_text(n, source))
                    .unwrap_or_default();
                let known_module = TYPED_DICT_MODULES.contains(&module.as_str());
                let mut inner = stmt.walk();
                for child in stmt.named_children(&mut inner) {
                    // module_name 自身は名前リストではないので飛ばす。
                    if child.child_by_field_name("module_name").is_some()
                        || Some(child.id())
                            == stmt.child_by_field_name("module_name").map(|n| n.id())
                    {
                        continue;
                    }
                    match child.kind() {
                        "wildcard_import" => return None,
                        "dotted_name" => {
                            let text = node_text(child, source)?;
                            if text == "TypedDict" {
                                if !known_module {
                                    return None;
                                }
                                names.insert(TypedDictName::Bare(text));
                            }
                        }
                        "aliased_import" => {
                            let original = child
                                .child_by_field_name("name")
                                .and_then(|n| node_text(n, source))?;
                            if original == "TypedDict" {
                                if !known_module {
                                    return None;
                                }
                                let alias = child
                                    .child_by_field_name("alias")
                                    .and_then(|n| node_text(n, source))?;
                                names.insert(TypedDictName::Bare(alias));
                            }
                        }
                        _ => {}
                    }
                }
            }
            "import_statement" => {
                let mut inner = stmt.walk();
                for child in stmt.named_children(&mut inner) {
                    match child.kind() {
                        "dotted_name" => {
                            let text = node_text(child, source)?;
                            if TYPED_DICT_MODULES.contains(&text.as_str()) {
                                names.insert(TypedDictName::Qualified { module_alias: text });
                            }
                        }
                        "aliased_import" => {
                            let original = child
                                .child_by_field_name("name")
                                .and_then(|n| node_text(n, source))?;
                            if TYPED_DICT_MODULES.contains(&original.as_str()) {
                                let alias = child
                                    .child_by_field_name("alias")
                                    .and_then(|n| node_text(n, source))?;
                                names.insert(TypedDictName::Qualified {
                                    module_alias: alias,
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Some(names)
}

/// TypedDict を指すと認識した名前。
///
/// 基底クラスとの照合に使うテキスト (`base_text`) と、shadow 検査で守る対象は別物になる。
/// `import typing as t` の場合、基底は `t.TypedDict` と書かれるが、守るべきは
/// 「`t` がちょうど 1 回だけ束縛されている」ことと「`t.TypedDict` が書き換えられていない」ことの両方。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum TypedDictName {
    /// `from typing import TypedDict [as TD]`。
    Bare(String),
    /// `import typing [as t]`。
    Qualified { module_alias: String },
}

impl TypedDictName {
    fn base_text(&self) -> String {
        match self {
            Self::Bare(name) => name.clone(),
            Self::Qualified { module_alias } => format!("{module_alias}.TypedDict"),
        }
    }

    /// この名前が本当に `typing.TypedDict` を指していると言えるか。
    fn is_provable(&self, bindings: &BindingCounts) -> bool {
        match self {
            Self::Bare(name) => bindings.count(name) == 1,
            Self::Qualified { module_alias } => {
                // alias 自体が 1 度だけ束縛され、かつ `t.TypedDict = Fake` のような
                // 属性への書き込みが無いこと。前者だけでは module のメンバ差し替えを見逃す。
                bindings.count(module_alias) == 1 && !bindings.attribute_written(&self.base_text())
            }
        }
    }
}

/// ファイル内で各名前が束縛された回数と、書き換えられた属性の集合。
///
/// 「代入だけを見る」実装では `class TypedDict:` / `def TypedDict():` / 二重 import /
/// `del TypedDict` / `match` の capture による shadow を取りこぼす。**列挙をやめて構造で拾う** —
/// 任意ノードの `left` / `name` / `alias` フィールドに現れた identifier を数える。これで
/// class/function 定義・import・代入・`for` の左辺・`as` 束縛がまとめて入る。
/// `case_pattern` と `delete_statement` は配下を丸ごと束縛イベントとして数える。
///
/// 過剰に数える形 (default 引数名など) もあるが、過剰側は「回数が 1 でない → 分類しない」に
/// 倒れるだけで安全。逆に `keyword_argument` の `name` は呼び出し時のラベルであって束縛では
/// ないため除外する (これを数えると `f(Base=1)` だけで無関係な分類が消えてしまう)。
#[derive(Debug, Default)]
struct BindingCounts {
    counts: std::collections::HashMap<String, usize>,
    /// `t.TypedDict = Fake` のように書き換えられた属性の完全名。
    attribute_writes: HashSet<String>,
    /// `globals()[...] = x` / `setattr` / `exec` / `eval` など、静的に追えない
    /// namespace 操作がファイル内にあるか。あれば一切分類しない。
    dynamic_namespace_op: bool,
}

impl BindingCounts {
    fn count(&self, name: &str) -> usize {
        self.counts.get(name).copied().unwrap_or(0)
    }

    fn add(&mut self, name: String) {
        *self.counts.entry(name).or_insert(0) += 1;
    }

    fn attribute_written(&self, dotted: &str) -> bool {
        self.attribute_writes.contains(dotted)
    }
}

/// 静的に追えない namespace 操作を行う組み込み。1 つでも呼ばれていたら分類しない。
const DYNAMIC_NAMESPACE_BUILTINS: [&str; 6] =
    ["globals", "locals", "vars", "setattr", "exec", "eval"];

fn collect_binding_counts(root: tree_sitter::Node<'_>, source: &[u8]) -> BindingCounts {
    let mut counts = BindingCounts::default();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        // `del X` は束縛を消し、`case X:` は capture として束縛する。どちらも
        // 「以降 X が何を指すか追えない」ため束縛イベントとして数え、回数 1 の要求から外す。
        // ただし属性への書き込み記録は `del t.TypedDict` だけ — `case t.TypedDict:` は
        // 値パターンの照合であって書き換えではない。
        if node.kind() == "delete_statement" {
            collect_identifiers(node, source, &mut counts, true);
        }
        if node.kind() == "case_pattern" {
            collect_identifiers(node, source, &mut counts, false);
        }
        if node.kind() == "call"
            && let Some(func) = node.child_by_field_name("function")
            && let Some(text) = node_text(func, source)
            && DYNAMIC_NAMESPACE_BUILTINS.contains(&text.as_str())
        {
            counts.dynamic_namespace_op = true;
        }
        // `aliased_import` の `name` / `alias` は親の import 文側で 1 度だけ数える
        // (ここでも数えると alias が 2 回計上され、正当な `import X as Y` を弾いてしまう)。
        // `keyword_argument` の `name` は呼び出しラベルであって束縛ではない。
        let skip_fields = matches!(node.kind(), "aliased_import" | "keyword_argument");
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                // フィールドは繰り返し現れる (`from x import a, b` の `name`)。
                // `child_by_field_name` は最初の 1 つしか返さないため cursor で全件見る。
                if !skip_fields && matches!(cursor.field_name(), Some("left" | "name" | "alias")) {
                    collect_identifiers(child, source, &mut counts, true);
                }
                if child.is_named() {
                    stack.push(child);
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }
    counts
}

/// 束縛位置のノードから identifier をすべて拾う (tuple/list 分解代入も辿る)。
///
/// `record_attribute_writes` は「このノードが書き込み位置か」。代入左辺や `del` では真、
/// `case` の値パターン照合では偽。
fn collect_identifiers(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    counts: &mut BindingCounts,
    record_attribute_writes: bool,
) {
    match node.kind() {
        // `t.TypedDict = Fake` / `del t.TypedDict` はローカル名を束縛しないが、
        // module のメンバを差し替える。正規化した完全名を記録して
        // qualified import の証明を無効化できるようにする。
        "attribute" => {
            if record_attribute_writes && let Some(path) = attribute_path(node, source) {
                counts.attribute_writes.insert(path);
            }
        }
        // `d[k] = v` はローカル名を束縛しない。`globals()[...] = x` は call 側で検出済み。
        "subscript" => {}
        "identifier" => {
            if let Some(text) = node_text(node, source) {
                counts.add(text);
            }
        }
        "dotted_name" => {
            // `import typing.abc` が束縛するのは先頭の `typing`。
            // `from x import a` の `a` は 1 セグメントなので同じ扱いで足りる。
            if let Some(first) = node.named_child(0)
                && let Some(text) = node_text(first, source)
            {
                counts.add(text);
            }
        }
        // `import X as Y` / `from m import X as Y` がローカルに束縛するのは alias だけ。
        "aliased_import" => {
            if let Some(alias) = node.child_by_field_name("alias") {
                collect_identifiers(alias, source, counts, record_attribute_writes);
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                collect_identifiers(child, source, counts, record_attribute_writes);
            }
        }
    }
}

/// `attribute` ノードを空白・括弧に依存しない `a.b.c` 形式へ正規化する。
/// 生テキストのまま比較すると `t . TypedDict = Fake` や `(t).TypedDict = Fake` を取りこぼす。
fn attribute_path(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" => node_text(node, source),
        "attribute" => {
            let object = node.child_by_field_name("object")?;
            let attribute = node.child_by_field_name("attribute")?;
            Some(format!(
                "{}.{}",
                attribute_path(object, source)?,
                node_text(attribute, source)?
            ))
        }
        "parenthesized_expression" => attribute_path(node.named_child(0)?, source),
        _ => None,
    }
}

/// module 直下の `class_definition` を集める。
///
/// デコレータ付きクラスは `None` にして分類自体を諦める。デコレータは公開名を別オブジェクトへ
/// 差し替えられるため、構文上 TypedDict を継承していても公開される値が TypedDict とは限らない。
fn module_level_classes<'a>(root: tree_sitter::Node<'a>) -> Option<Vec<tree_sitter::Node<'a>>> {
    let mut classes = Vec::new();
    let mut cursor = root.walk();
    for stmt in root.named_children(&mut cursor) {
        match stmt.kind() {
            "class_definition" => classes.push(stmt),
            "decorated_definition" => {
                if let Some(inner) = stmt.child_by_field_name("definition")
                    && inner.kind() == "class_definition"
                {
                    return None;
                }
            }
            _ => {}
        }
    }
    Some(classes)
}

/// 指定名のクラスが module 直下にちょうど 1 つあるときだけ返す。
/// 同名クラスの再定義があると old/new のどちらを見ているか確定できない。
fn unique_class_named<'a>(
    classes: &[tree_sitter::Node<'a>],
    source: &[u8],
    class_name: &str,
) -> Option<tree_sitter::Node<'a>> {
    let mut found = None;
    for &class in classes {
        let name = class
            .child_by_field_name("name")
            .and_then(|n| node_text(n, source));
        if name.as_deref() == Some(class_name) {
            if found.is_some() {
                return None;
            }
            found = Some(class);
        }
    }
    found
}

/// 「基底がすべて認識済み TypedDict または証明済みクラス」を満たすクラスを不動点で確定する。
/// 未知の基底が 1 つでも混ざるクラスは証明しない。
fn prove_typed_dict_classes(
    classes: &[tree_sitter::Node<'_>],
    source: &[u8],
    typed_dict_names: &HashSet<TypedDictName>,
) -> HashSet<usize> {
    let known_bases: HashSet<String> = typed_dict_names.iter().map(|n| n.base_text()).collect();
    let mut proven_ids: HashSet<usize> = HashSet::new();
    let mut proven_names: HashSet<String> = HashSet::new();
    loop {
        let mut grew = false;
        for &class in classes {
            if proven_ids.contains(&class.id()) {
                continue;
            }
            let Some(header) = class_header(class, source) else {
                continue;
            };
            if header.bases.is_empty() {
                continue;
            }
            let all_known = header
                .bases
                .iter()
                .all(|b| known_bases.contains(b) || proven_names.contains(b));
            if !all_known {
                continue;
            }
            let Some(name) = class
                .child_by_field_name("name")
                .and_then(|n| node_text(n, source))
            else {
                continue;
            };
            proven_ids.insert(class.id());
            proven_names.insert(name);
            grew = true;
        }
        if !grew {
            return proven_ids;
        }
    }
}

/// クラスヘッダから読み取った基底クラス名・型パラメータ・実効 total。
struct ClassHeader {
    bases: Vec<String>,
    /// PEP 695 の型パラメータ (`class B[T](TypedDict)`)。無ければ `None`。
    type_params: Option<String>,
    total: bool,
}

/// `class X(Base, total=False)` の引数リストを解析する。
///
/// `None` に倒す形:
/// - 基底リストが無い (`class X:` — TypedDict ではありえない)
/// - `total` 以外の keyword 引数がある / `total` が複数ある
/// - `total` の値が `True` / `False` リテラルでない (`total=FLAG` / `total=0` / `total=not DEBUG`)
/// - `*bases` / `**options` の展開がある
/// - 基底が identifier / attribute 以外 (`Generic[T]` のような subscript を含む)
fn class_header(class: tree_sitter::Node<'_>, source: &[u8]) -> Option<ClassHeader> {
    let type_params = match class.child_by_field_name("type_parameters") {
        Some(tp) => Some(node_text(tp, source)?),
        None => None,
    };
    let args = class.child_by_field_name("superclasses")?;
    let mut bases = Vec::new();
    let mut total: Option<bool> = None;
    let mut cursor = args.walk();
    for arg in args.named_children(&mut cursor) {
        match arg.kind() {
            "keyword_argument" => {
                let key = arg
                    .child_by_field_name("name")
                    .and_then(|n| node_text(n, source))?;
                if key != "total" || total.is_some() {
                    return None;
                }
                let value = arg.child_by_field_name("value")?;
                total = Some(match value.kind() {
                    "true" => true,
                    "false" => false,
                    _ => return None,
                });
            }
            "dictionary_splat" | "list_splat" => return None,
            "identifier" | "attribute" => bases.push(node_text(arg, source)?),
            "comment" => {}
            _ => return None,
        }
    }
    Some(ClassHeader {
        bases,
        type_params,
        total: total.unwrap_or(true),
    })
}

/// クラス本体が宣言する `名前: 型注釈` を集める。
///
/// TypedDict の本体はフィールド宣言 (と docstring / `pass`) しか取りえない。メソッド定義や
/// 条件分岐、`名前: 型 = 既定値` のような想定外の要素が混ざれば `None` に倒す。
fn class_own_fields(
    class: tree_sitter::Node<'_>,
    source: &[u8],
) -> Option<BTreeMap<String, String>> {
    let body = class.child_by_field_name("body")?;
    let mut fields = BTreeMap::new();
    let mut cursor = body.walk();
    for stmt in body.named_children(&mut cursor) {
        match stmt.kind() {
            "pass_statement" | "comment" => {}
            "expression_statement" => {
                let inner = stmt.named_child(0)?;
                match inner.kind() {
                    // docstring
                    "string" => {}
                    "assignment" => {
                        let left = inner.child_by_field_name("left")?;
                        if left.kind() != "identifier" {
                            return None;
                        }
                        // TypedDict のフィールドは既定値を持てない。`x: int = 1` は想定外。
                        if inner.child_by_field_name("right").is_some() {
                            return None;
                        }
                        let ty = inner.child_by_field_name("type")?;
                        let name = node_text(left, source)?;
                        let ty_text = super::normalize_signature_whitespace(
                            source.get(ty.start_byte()..ty.end_byte())?,
                        );
                        if fields.insert(name, ty_text).is_some() {
                            // 同名フィールドの重複宣言は解釈が割れるため諦める。
                            return None;
                        }
                    }
                    _ => return None,
                }
            }
            _ => return None,
        }
    }
    Some(fields)
}

/// ノードの UTF-8 テキストを取り出す。非 UTF-8 / 範囲外は `None`。
fn node_text(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let bytes = source.get(node.start_byte()..node.end_byte())?;
    std::str::from_utf8(bytes).ok().map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::parser;

    fn fact(src: &str, class_name: &str) -> Option<TypedDictFact> {
        let bytes = src.as_bytes();
        let tree = parser::parse_source(bytes, LangId::Python).ok()?;
        analyze_typed_dict(tree.root_node(), bytes, class_name)
    }

    const IMPORT: &str = "from typing import TypedDict\n";

    #[test]
    fn total_false_is_read_as_effective_false() {
        let f = fact(
            &format!("{IMPORT}\nclass B(TypedDict, total=False):\n    a: int\n"),
            "B",
        )
        .expect("分類できるはず");
        assert!(!f.effective_total);
        assert_eq!(f.own_fields.get("a").map(String::as_str), Some("int"));
    }

    #[test]
    fn omitted_total_is_effective_true() {
        let f = fact(&format!("{IMPORT}\nclass B(TypedDict):\n    a: int\n"), "B")
            .expect("分類できるはず");
        assert!(f.effective_total);
    }

    #[test]
    fn explicit_total_true_is_effective_true() {
        let f = fact(
            &format!("{IMPORT}\nclass B(TypedDict, total=True):\n    a: int\n"),
            "B",
        )
        .expect("分類できるはず");
        assert!(f.effective_total);
    }

    #[test]
    fn aliased_typed_dict_import_is_recognized() {
        let f = fact(
            "from typing import TypedDict as TD\n\nclass B(TD, total=False):\n    a: int\n",
            "B",
        )
        .expect("alias でも証明できるはず");
        assert!(!f.effective_total);
    }

    #[test]
    fn module_qualified_typed_dict_is_recognized() {
        let f = fact(
            "import typing\n\nclass B(typing.TypedDict, total=False):\n    a: int\n",
            "B",
        )
        .expect("typing.TypedDict でも証明できるはず");
        assert!(!f.effective_total);
    }

    #[test]
    fn aliased_module_qualified_typed_dict_is_recognized() {
        let f = fact(
            "import typing as t\n\nclass B(t.TypedDict, total=False):\n    a: int\n",
            "B",
        )
        .expect("t.TypedDict でも証明できるはず");
        assert!(!f.effective_total);
    }

    #[test]
    fn typing_extensions_typed_dict_is_recognized() {
        let f = fact(
            "from typing_extensions import TypedDict\n\nclass B(TypedDict, total=False):\n    a: int\n",
            "B",
        )
        .expect("typing_extensions でも証明できるはず");
        assert!(!f.effective_total);
    }

    #[test]
    fn same_file_typed_dict_subclass_is_proven() {
        let f = fact(
            &format!("{IMPORT}\nclass A(TypedDict):\n    a: int\n\nclass B(A, total=False):\n    b: str\n"),
            "B",
        )
        .expect("同一ファイル内の TypedDict 継承は証明できるはず");
        assert!(!f.effective_total);
        // total は自クラス宣言分にしか作用しないため、継承した `a` は含めない。
        assert_eq!(f.own_fields.len(), 1);
        assert!(f.own_fields.contains_key("b"));
    }

    #[test]
    fn unknown_import_source_is_rejected() {
        assert!(
            fact(
                "from mylib import TypedDict\n\nclass B(TypedDict, total=False):\n    a: int\n",
                "B",
            )
            .is_none()
        );
    }

    #[test]
    fn star_import_is_rejected() {
        assert!(
            fact(
                "from typing import *\n\nclass B(TypedDict, total=False):\n    a: int\n",
                "B",
            )
            .is_none()
        );
    }

    #[test]
    fn rebound_typed_dict_name_is_rejected() {
        assert!(
            fact(
                &format!(
                    "{IMPORT}TypedDict = object\n\nclass B(TypedDict, total=False):\n    a: int\n"
                ),
                "B",
            )
            .is_none()
        );
    }

    #[test]
    fn plain_class_without_typed_dict_base_is_rejected() {
        assert!(
            fact(
                &format!(
                    "{IMPORT}\nclass Base:\n    pass\n\nclass B(Base, total=False):\n    a: int\n"
                ),
                "B",
            )
            .is_none()
        );
    }

    #[test]
    fn unknown_extra_base_is_rejected() {
        assert!(
            fact(
                &format!("{IMPORT}\nclass B(TypedDict, Mixin, total=False):\n    a: int\n"),
                "B",
            )
            .is_none()
        );
    }

    #[test]
    fn subscript_base_is_rejected() {
        assert!(
            fact(
                &format!("{IMPORT}from typing import Generic, TypeVar\nT = TypeVar(\"T\")\n\nclass B(TypedDict, Generic[T], total=False):\n    a: int\n"),
                "B",
            )
            .is_none()
        );
    }

    #[test]
    fn non_literal_total_is_rejected() {
        assert!(
            fact(
                &format!("{IMPORT}FLAG = False\n\nclass B(TypedDict, total=FLAG):\n    a: int\n"),
                "B",
            )
            .is_none()
        );
    }

    #[test]
    fn numeric_total_is_rejected() {
        assert!(
            fact(
                &format!("{IMPORT}\nclass B(TypedDict, total=0):\n    a: int\n"),
                "B",
            )
            .is_none()
        );
    }

    #[test]
    fn splat_argument_is_rejected() {
        assert!(
            fact(
                &format!("{IMPORT}OPTS = {{}}\n\nclass B(TypedDict, **OPTS):\n    a: int\n"),
                "B",
            )
            .is_none()
        );
    }

    #[test]
    fn not_required_anywhere_in_file_is_rejected() {
        assert!(
            fact(
                "from typing import TypedDict, NotRequired\n\nclass B(TypedDict, total=False):\n    a: int\n    b: NotRequired[str]\n",
                "B",
            )
            .is_none()
        );
    }

    #[test]
    fn field_with_default_is_rejected() {
        assert!(
            fact(
                &format!("{IMPORT}\nclass B(TypedDict, total=False):\n    a: int = 1\n"),
                "B",
            )
            .is_none()
        );
    }

    #[test]
    fn method_in_body_is_rejected() {
        assert!(
            fact(
                &format!("{IMPORT}\nclass B(TypedDict, total=False):\n    a: int\n\n    def m(self) -> None:\n        pass\n"),
                "B",
            )
            .is_none()
        );
    }

    #[test]
    fn empty_body_is_rejected() {
        assert!(
            fact(
                &format!("{IMPORT}\nclass B(TypedDict, total=False):\n    pass\n"),
                "B"
            )
            .is_none()
        );
    }

    #[test]
    fn docstring_only_body_is_rejected() {
        assert!(
            fact(
                &format!("{IMPORT}\nclass B(TypedDict, total=False):\n    \"\"\"doc\"\"\"\n"),
                "B",
            )
            .is_none()
        );
    }

    #[test]
    fn docstring_with_field_is_accepted() {
        let f = fact(
            &format!(
                "{IMPORT}\nclass B(TypedDict, total=False):\n    \"\"\"doc\"\"\"\n    a: int\n"
            ),
            "B",
        )
        .expect("docstring があってもフィールドがあれば分類できる");
        assert_eq!(f.own_fields.len(), 1);
    }

    #[test]
    fn duplicate_class_definition_is_rejected() {
        assert!(
            fact(
                &format!("{IMPORT}\nclass B(TypedDict, total=False):\n    a: int\n\nclass B(TypedDict):\n    a: int\n"),
                "B",
            )
            .is_none()
        );
    }

    #[test]
    fn nested_class_is_not_module_level() {
        assert!(
            fact(
                &format!(
                    "{IMPORT}\nclass Outer:\n    class B(TypedDict, total=False):\n        a: int\n"
                ),
                "B",
            )
            .is_none()
        );
    }

    #[test]
    fn conditional_import_is_not_collected() {
        assert!(
            fact(
                "import sys\nif sys.version_info >= (3, 11):\n    from typing import TypedDict\n\nclass B(TypedDict, total=False):\n    a: int\n",
                "B",
            )
            .is_none()
        );
    }

    #[test]
    fn missing_import_is_rejected() {
        assert!(fact("class B(TypedDict, total=False):\n    a: int\n", "B").is_none());
    }

    #[test]
    fn class_without_bases_is_rejected() {
        assert!(fact(&format!("{IMPORT}\nclass B:\n    a: int\n"), "B").is_none());
    }

    // --- レビュー指摘 (2026-08-19) で追加した shadow / ヘッダ一致 / 平文フィールド証明 ---

    #[test]
    fn class_shadowing_typed_dict_name_is_rejected() {
        // `from typing import TypedDict` の後にローカル class TypedDict を定義すると、
        // 基底に書かれた `TypedDict` は本物ではない。
        assert!(
            fact(
                &format!("{IMPORT}\n\nclass TypedDict:\n    pass\n\n\nclass B(TypedDict, total=False):\n    a: int\n"),
                "B",
            )
            .is_none()
        );
    }

    #[test]
    fn function_shadowing_typed_dict_name_is_rejected() {
        assert!(
            fact(
                &format!("{IMPORT}\n\ndef TypedDict():\n    pass\n\n\nclass B(TypedDict, total=False):\n    a: int\n"),
                "B",
            )
            .is_none()
        );
    }

    #[test]
    fn deleted_typed_dict_name_is_rejected() {
        assert!(
            fact(
                &format!("{IMPORT}del TypedDict\n\nclass B(TypedDict, total=False):\n    a: int\n"),
                "B",
            )
            .is_none()
        );
    }

    #[test]
    fn rebound_module_alias_is_rejected() {
        assert!(
            fact(
                "import typing as t\nt = object()\n\nclass B(t.TypedDict, total=False):\n    a: int\n",
                "B",
            )
            .is_none()
        );
    }

    #[test]
    fn rebound_proven_base_class_is_rejected() {
        assert!(
            fact(
                &format!("{IMPORT}\nclass A(TypedDict):\n    a: int\n\n\nA = object\n\n\nclass B(A, total=False):\n    b: str\n"),
                "B",
            )
            .is_none()
        );
    }

    #[test]
    fn duplicate_base_class_definition_is_rejected() {
        assert!(
            fact(
                &format!("{IMPORT}\nclass A(TypedDict):\n    a: int\n\n\nclass A(TypedDict):\n    a2: int\n\n\nclass B(A, total=False):\n    b: str\n"),
                "B",
            )
            .is_none()
        );
    }

    #[test]
    fn decorated_class_is_rejected() {
        // デコレータは公開名を別オブジェクトへ差し替えられるため、構文だけでは TypedDict と言えない。
        assert!(
            fact(
                &format!("{IMPORT}\n\ndef deco(c):\n    return c\n\n\n@deco\nclass B(TypedDict, total=False):\n    a: int\n"),
                "B",
            )
            .is_none()
        );
    }

    #[test]
    fn bases_are_recorded_for_equality_check() {
        let f = fact(
            &format!("{IMPORT}\nclass B(TypedDict, total=False):\n    a: int\n"),
            "B",
        )
        .expect("分類できるはず");
        assert_eq!(f.bases, vec!["TypedDict".to_string()]);
        assert_eq!(f.type_params, None);
    }

    #[test]
    fn type_parameters_are_recorded() {
        let f = fact(
            "from typing import TypedDict\n\nclass B[T](TypedDict, total=False):\n    a: int\n",
            "B",
        );
        // PEP 695 構文を parse できる場合は型パラメータを記録する。
        // parse できない tree-sitter バージョンでは None に倒れる (fail-closed) ので、
        // どちらでも「誤って総当たり分類しない」ことだけを保証する。
        if let Some(f) = f {
            assert_eq!(f.type_params.as_deref(), Some("[T]"));
        }
    }

    #[test]
    fn field_with_only_imported_types_is_rejected() {
        // `MyModel` が別モジュールで `NotRequired[str]` の別名である可能性を排除できない。
        assert!(
            fact(
                "from typing import TypedDict\nfrom mylib import MyModel\n\nclass B(TypedDict, total=False):\n    a: MyModel\n",
                "B",
            )
            .is_none()
        );
    }

    #[test]
    fn one_plain_builtin_field_is_enough() {
        let f = fact(
            "from typing import TypedDict\nfrom mylib import MyModel\n\nclass B(TypedDict, total=False):\n    a: int\n    b: MyModel\n",
            "B",
        )
        .expect("平文の組み込み型フィールドが 1 件あれば requiredness の反転を証明できる");
        assert!(!f.effective_total);
    }

    #[test]
    fn subscripted_builtin_field_is_plain() {
        let f = fact(
            &format!("{IMPORT}\nclass B(TypedDict, total=False):\n    a: list[str]\n"),
            "B",
        )
        .expect("list[str] は requiredness 修飾子ではありえない");
        assert!(!f.effective_total);
    }

    #[test]
    fn rebound_builtin_name_is_not_plain() {
        assert!(
            fact(
                &format!("{IMPORT}int = str\n\nclass B(TypedDict, total=False):\n    a: int\n"),
                "B",
            )
            .is_none()
        );
    }

    // --- レビュー 2 巡目 (2026-08-19) の指摘に対応 ---

    #[test]
    fn match_capture_rebinding_typed_dict_name_is_rejected() {
        // `case TypedDict:` は capture パターンで、`TypedDict` を束縛し直す。
        assert!(
            fact(
                &format!("{IMPORT}\n\nmatch value:\n    case TypedDict:\n        pass\n\n\nclass B(TypedDict, total=False):\n    a: int\n"),
                "B",
            )
            .is_none()
        );
    }

    #[test]
    fn rebound_qualified_typed_dict_member_is_rejected() {
        // alias 自体は 1 度しか束縛されないが、module のメンバが差し替えられている。
        assert!(
            fact(
                "import typing as t\n\n\nclass Fake:\n    pass\n\n\nt.TypedDict = Fake\n\nclass B(t.TypedDict, total=False):\n    a: int\n",
                "B",
            )
            .is_none()
        );
    }

    #[test]
    fn dynamic_namespace_manipulation_is_rejected() {
        // `globals()["int"] = ...` は静的に読んだ束縛を実行時に無効化できる。
        assert!(
            fact(
                &format!("{IMPORT}import typing\n\nglobals()[\"int\"] = typing.Any\n\nclass B(TypedDict, total=False):\n    a: int\n"),
                "B",
            )
            .is_none()
        );
    }

    #[test]
    fn setattr_call_anywhere_is_rejected() {
        assert!(
            fact(
                &format!("{IMPORT}import typing\n\nsetattr(typing, \"X\", 1)\n\nclass B(TypedDict, total=False):\n    a: int\n"),
                "B",
            )
            .is_none()
        );
    }

    #[test]
    fn rebound_qualified_member_with_odd_spacing_is_rejected() {
        // 生テキスト比較だと `t . TypedDict` / `(t).TypedDict` を取りこぼす。
        for src in [
            "import typing as t\n\n\nclass Fake:\n    pass\n\n\nt . TypedDict = Fake\n\nclass B(t.TypedDict, total=False):\n    a: int\n",
            "import typing as t\n\n\nclass Fake:\n    pass\n\n\n(t).TypedDict = Fake\n\nclass B(t.TypedDict, total=False):\n    a: int\n",
            "import typing as t\n\ndel t.TypedDict\n\nclass B(t.TypedDict, total=False):\n    a: int\n",
        ] {
            assert!(fact(src, "B").is_none(), "取りこぼした: {src}");
        }
    }

    #[test]
    fn case_value_pattern_is_not_an_attribute_write() {
        // `case t.TypedDict:` は値パターンの照合であって書き換えではない。
        // ただし match の capture 過剰検出で分類しない可能性はあるため、
        // ここでは「属性書き込みとして誤記録されない」ことだけを直接確かめる。
        let src = "import typing as t\n\nmatch value:\n    case t.TypedDict:\n        pass\n";
        let bytes = src.as_bytes();
        let tree = parser::parse_source(bytes, LangId::Python).expect("parse");
        let bindings = collect_binding_counts(tree.root_node(), bytes);
        assert!(!bindings.attribute_written("t.TypedDict"));
    }

    #[test]
    fn keyword_argument_label_is_not_counted_as_binding() {
        // `f(Base=1)` の `Base` は呼び出しラベルであって束縛ではない。
        // これを数えていた版では、無関係な呼び出し 1 つで分類が消えていた。
        let f = fact(
            &format!("{IMPORT}\n\ndef f(**kw):\n    return kw\n\n\nclass Base(TypedDict):\n    a: int\n\n\nf(Base=1)\n\n\nclass B(Base, total=False):\n    b: str\n"),
            "B",
        )
        .expect("keyword ラベルは束縛ではないので分類できるはず");
        assert!(!f.effective_total);
    }

    #[test]
    fn typed_dict_imported_after_other_names_is_recognized() {
        // `from typing import Any, TypedDict` のように 2 番目以降でも認識できること
        // (繰り返しフィールドを cursor で全件見ているかの回帰)。
        let f = fact(
            "from typing import Any, TypedDict\n\nclass B(TypedDict, total=False):\n    a: int\n",
            "B",
        )
        .expect("import リストの 2 番目でも認識できるはず");
        assert!(!f.effective_total);
    }
}
