//! 関数コンポーネントの inline object 型の props から、省略可能プロパティだけを消した変更と、
//! リポジトリ内の利用の証拠 (Issue #208)。
//!
//! TypeScript の余剰プロパティの検査は、書いたキーにしか掛からない。消したキーを書かない
//! JSX 要素と object リテラルは、旧新の props の型で同じ型検査の結果になる。

use std::collections::{BTreeSet, HashSet};

use tree_sitter::Node;

use super::removed_params::{
    OptionalRemovalCache, UseRule, prove_repository_uses, unique_function_declaration,
};
use super::type_dependencies::{DependencyEnvironment, type_dependencies_unchanged};
use super::*;
use crate::models::impact::DiffFile;

pub(crate) struct RemovedParameterProps {
    pub(crate) parameter: usize,
    pub(crate) names: HashSet<String>,
}

/// 互換性ではなく、変更行にも課す「消したキーを渡さない」義務を取り出す。
pub(crate) fn removed_parameter_props(
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
) -> Result<Vec<RemovedParameterProps>, ()> {
    if site.kind != "function" || site.name.contains('.') || site.lang_in(TS_ONLY_LANGS).is_none() {
        return Ok(Vec::new());
    }
    let src = sources.get(site).ok_or(())?;
    let (old_tree, new_tree) = src.parse_pair(site.lang_id.ok_or(())?).ok_or(())?;
    let old_fn =
        super::super::js_ts_call_arity::unique_function(old_tree.root_node(), &src.old, site.name)
            .ok_or(())?;
    let new_fn =
        super::super::js_ts_call_arity::unique_function(new_tree.root_node(), &src.new, site.name)
            .ok_or(())?;
    fn parameters(function: Node<'_>) -> Option<Vec<Node<'_>>> {
        let parameters = function.child_by_field_name("parameters")?;
        let mut cursor = parameters.walk();
        Some(
            parameters
                .named_children(&mut cursor)
                .filter(|n| !n.is_extra())
                .collect::<Vec<_>>(),
        )
    }
    let old_parameters = parameters(old_fn).ok_or(())?;
    let new_parameters = parameters(new_fn).ok_or(())?;
    let mut result = Vec::new();
    for (parameter, old) in old_parameters.iter().enumerate() {
        let Some(old_type) = old.child_by_field_name("type").and_then(inline_object_type) else {
            continue;
        };
        let new_type = new_parameters
            .get(parameter)
            .and_then(|n| n.child_by_field_name("type"))
            .and_then(inline_object_type)
            .ok_or(())?;
        if node_signature_tokens(old_type, &src.old) == node_signature_tokens(new_type, &src.new) {
            continue;
        }
        let old = members(old_type, &src.old).ok_or(())?;
        let new: HashSet<_> = members(new_type, &src.new)
            .ok_or(())?
            .into_iter()
            .map(|m| m.name)
            .collect();
        let names = old
            .into_iter()
            .map(|m| m.name)
            .filter(|name| !new.contains(name))
            .collect::<HashSet<_>>();
        if !names.is_empty() {
            result.push(RemovedParameterProps { parameter, names });
        }
    }
    // rest / this / generic 等の制約は、削除キーがある場合だけ課す。無関係な引数追加には
    // 従来の arity と変更行の証明をそのまま使う。
    if !result.is_empty() {
        unique_function_declaration(old_tree.root_node(), &src.old, site.name).ok_or(())?;
        unique_function_declaration(new_tree.root_node(), &src.new, site.name).ok_or(())?;
    }
    Ok(result)
}

pub(crate) fn call_omits_removed_props(
    identifier: Node<'_>,
    source: &[u8],
    removals: &[RemovedParameterProps],
) -> bool {
    if crate::engine::jsx::jsx_name_opening_tag(identifier, source).is_some() {
        return removals.iter().all(|removal| {
            removal.parameter == 0
                && crate::engine::jsx::caller_shape(identifier, source, true)
                    .is_some_and(|shape| !shape.children && shape.omits(&removal.names))
        });
    }
    let Some(call) = identifier
        .parent()
        .filter(|n| n.kind() == "call_expression")
    else {
        return false;
    };
    if call.child_by_field_name("function").map(|n| n.id()) != Some(identifier.id())
        || call.child_by_field_name("type_arguments").is_some()
    {
        return false;
    }
    let Some(arguments) = call
        .child_by_field_name("arguments")
        .filter(|n| n.kind() == "arguments")
    else {
        return false;
    };
    let mut cursor = arguments.walk();
    let values = arguments
        .named_children(&mut cursor)
        .filter(|n| !n.is_extra())
        .collect::<Vec<_>>();
    // spread で位置がずれる場合も、渡したキーは確定できない。
    !values.iter().any(|n| n.kind() == "spread_element")
        && removals.iter().all(|removal| {
            values.get(removal.parameter).is_none_or(|value| {
                super::removed_params::object_omits(*value, source, &removal.names)
            })
        })
}

pub(crate) fn detect_optional_object_props_removed_compatible_mod(
    index: &ApiRefIndex,
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
    cache: &mut OptionalRemovalCache,
    diff_files: &[DiffFile],
) -> Option<CompatibleApiModification> {
    if site.kind != "function" || site.name.contains('.') || site.old_path != site.new_path {
        return None;
    }
    let lang = site.lang_in(TS_ONLY_LANGS)?;
    let src = sources.get(site)?;
    let (old_tree, new_tree) = src.parse_pair(lang)?;
    let (old_root, new_root) = (old_tree.root_node(), new_tree.root_node());
    let removal = props_removal(old_root, &src.old, new_root, &src.new, site.name)?;
    let refs = index.complete_refs_for(site.name)?;
    if refs.is_empty() {
        return None;
    }
    // 環境の監査を先に確かめる。成り立たなければ、在庫の外の利用を探さない。
    {
        let evidence = cache.evidence(site.dir)?;
        // JSX の型環境 (設定・ソース・自動の runtime と型の React の出どころ) が標準であることを
        // 求める。
        if !evidence.repository.as_ref()?.jsx_standard() {
            return None;
        }
        let packages = evidence.relaxation()?;
        let changed_paths: BTreeSet<String> = diff_files
            .iter()
            .flat_map(|file| [file.old_path.as_str(), file.new_path.as_str()])
            .filter(|path| *path != "/dev/null")
            .map(crate::git_support::normalize_workspace_separators)
            .collect();
        let registry_dependency = |specifier: &str| packages.is_registry_dependency(specifier);
        let declared_globally = |name: &str| evidence.declares_global(name);
        let environment = DependencyEnvironment {
            target_path: site.new_path,
            changed_paths: &changed_paths,
            registry_dependency: &registry_dependency,
            declared_globally: &declared_globally,
        };
        if !type_dependencies_unchanged(
            &environment,
            old_root,
            &src.old,
            new_root,
            &src.new,
            &removal.type_roots,
        ) {
            return None;
        }
    }
    // 在庫は基本の集合と、その外で名前が現れるか宣言のファイルに依存し得るファイル。
    let (inventory, evidence) = cache.inventory(site.dir, site.name, site.new_path)?;
    // 在庫は元のソースを読んでいるので、元の新版と照合する。
    if !inventory.has_source(site.new_path, src.original_new()) {
        return None;
    }
    prove_repository_uses(
        refs,
        &inventory,
        evidence.relaxation(),
        site.new_path,
        site.name,
        UseRule::PropsWithout {
            removed: &removal.removed,
            calls_allowed: removal.explicit_return,
        },
    )?;
    Some(site.compatible("optional_object_props_removed"))
}

struct PropsRemoval<'tree> {
    /// 消した省略可能プロパティの名前。
    removed: HashSet<String>,
    /// 旧新で同じ明示の戻り値型がある。直接呼び出しは、このときだけ証明に使う。
    explicit_return: bool,
    /// 型の依存をたどる起点 (新の側の残った props の型と戻り値型)。
    type_roots: Vec<Node<'tree>>,
}

/// 宣言の旧新の差が、唯一の引数の inline object 型から省略可能プロパティを消し、その分割代入の
/// 束縛を消しただけかを確かめる。
fn props_removal<'tree>(
    old_root: Node<'_>,
    old_source: &[u8],
    new_root: Node<'tree>,
    new_source: &[u8],
    name: &str,
) -> Option<PropsRemoval<'tree>> {
    let (old_fn, old_statement) = unique_function_declaration(old_root, old_source, name)?;
    let (new_fn, new_statement) = unique_function_declaration(new_root, new_source, name)?;
    if old_statement.kind() != "export_statement" || new_statement.kind() != "export_statement" {
        return None;
    }
    let header = |statement: Node<'_>, function: Node<'_>, source: &[u8]| {
        signature_tokens_in_range(
            statement,
            source,
            statement.start_byte(),
            function.child_by_field_name("parameters")?.start_byte(),
        )
    };
    let old_parts = ts_function_signature_parts(old_fn, old_source)?;
    let new_parts = ts_function_signature_parts(new_fn, new_source)?;
    if header(old_statement, old_fn, old_source)? != header(new_statement, new_fn, new_source)?
        || old_parts.head != new_parts.head
        || old_parts.tail != new_parts.tail
        || old_parts.params.len() != 1
        || new_parts.params.len() != 1
    {
        return None;
    }
    let old_param = only_parameter(old_fn)?;
    let new_param = only_parameter(new_fn)?;
    if old_param.kind() != new_param.kind()
        || !matches!(
            old_param.kind(),
            "required_parameter" | "optional_parameter"
        )
        || old_param.child_by_field_name("value").is_some()
        || new_param.child_by_field_name("value").is_some()
    {
        return None;
    }
    let old_pattern = old_param.child_by_field_name("pattern")?;
    let new_pattern = new_param.child_by_field_name("pattern")?;
    let old_annotation = old_param.child_by_field_name("type")?;
    let new_annotation = new_param.child_by_field_name("type")?;
    let old_type = inline_object_type(old_annotation)?;
    let new_type = inline_object_type(new_annotation)?;
    // 束縛と型の外 (修飾子・区切り・`:`) は変えない。
    for (old_start, old_end, new_start, new_end) in [
        (
            old_param.start_byte(),
            old_pattern.start_byte(),
            new_param.start_byte(),
            new_pattern.start_byte(),
        ),
        (
            old_pattern.end_byte(),
            old_type.start_byte(),
            new_pattern.end_byte(),
            new_type.start_byte(),
        ),
        (
            old_type.end_byte(),
            old_param.end_byte(),
            new_type.end_byte(),
            new_param.end_byte(),
        ),
    ] {
        if signature_tokens_in_range(old_param, old_source, old_start, old_end)?
            != signature_tokens_in_range(new_param, new_source, new_start, new_end)?
        {
            return None;
        }
    }
    let removed = removed_optional_members(old_type, old_source, new_type, new_source)?;
    if !pattern_drops_only(old_pattern, old_source, new_pattern, new_source, &removed) {
        return None;
    }
    let old_return = old_fn.child_by_field_name("return_type");
    let new_return = new_fn.child_by_field_name("return_type");
    let explicit_return = match (old_return, new_return) {
        (Some(_), Some(_)) => true,
        (None, None) => false,
        _ => return None,
    };
    if !explicit_return {
        // 注釈の無い戻り値は本文から推論される。旧新とも JSX 要素や単純な値を直接返し、その
        // 分類が一致するときだけ同じ型とみなす (`return value` や呼び出しは証明しない)。
        if crate::engine::jsx::has_jsx_pragma(old_root, old_source)
            || crate::engine::jsx::has_jsx_pragma(new_root, new_source)
        {
            return None;
        }
        let old_body = old_fn.child_by_field_name("body")?;
        let new_body = new_fn.child_by_field_name("body")?;
        if crate::engine::jsx::inferred_returns(old_body, old_source)?
            != crate::engine::jsx::inferred_returns(new_body, new_source)?
        {
            return None;
        }
    }
    let mut type_roots = vec![new_type];
    type_roots.extend(new_return);
    Some(PropsRemoval {
        removed,
        explicit_return,
        type_roots,
    })
}

fn only_parameter(function: Node<'_>) -> Option<Node<'_>> {
    let parameters = function.child_by_field_name("parameters")?;
    let mut cursor = parameters.walk();
    let parameters: Vec<_> = parameters
        .named_children(&mut cursor)
        .filter(|child| !child.is_extra())
        .collect();
    let [parameter] = parameters.as_slice() else {
        return None;
    };
    Some(*parameter)
}

/// 型注釈がコメントを除いて inline の object 型 1 つだけならそのノード。
fn inline_object_type(annotation: Node<'_>) -> Option<Node<'_>> {
    if annotation.kind() != "type_annotation" {
        return None;
    }
    let mut cursor = annotation.walk();
    let types: Vec<_> = annotation
        .named_children(&mut cursor)
        .filter(|child| !child.is_extra())
        .collect();
    let [ty] = types.as_slice() else {
        return None;
    };
    (ty.kind() == "object_type").then_some(*ty)
}

struct Member<'tree> {
    name: String,
    node: Node<'tree>,
    tokens: SigTokens,
}

/// object 型のメンバー (コメントを除く)。すべてが通常の名前の property_signature で、名前が
/// 一意のときだけ返す。method・call・construct・index signature があれば None。
fn members<'tree>(ty: Node<'tree>, source: &[u8]) -> Option<Vec<Member<'tree>>> {
    let mut names = HashSet::new();
    let mut result = Vec::new();
    let mut cursor = ty.walk();
    for member in ty.named_children(&mut cursor) {
        if member.is_extra() {
            continue;
        }
        if member.kind() != "property_signature" {
            return None;
        }
        let key = member.child_by_field_name("name")?;
        if key.kind() != "property_identifier" {
            return None;
        }
        let name = key.utf8_text(source).ok()?;
        if name.is_empty() || name.contains('\\') || !names.insert(name.to_owned()) {
            return None;
        }
        result.push(Member {
            name: name.to_owned(),
            node: member,
            tokens: node_signature_tokens(member, source)?,
        });
    }
    Some(result)
}

/// 新のメンバー列が、旧の列から `?` 付きのメンバーを除いたものとトークンで一致する (順序も)
/// とき、除いたメンバーの名前を返す。除いたものが無い・新に同じ名前が残る場合は None。
fn removed_optional_members(
    old_type: Node<'_>,
    old_source: &[u8],
    new_type: Node<'_>,
    new_source: &[u8],
) -> Option<HashSet<String>> {
    let old = members(old_type, old_source)?;
    let new = members(new_type, new_source)?;
    let new_names: HashSet<&str> = new.iter().map(|member| member.name.as_str()).collect();
    let mut removed = HashSet::new();
    let mut next = 0;
    for member in &old {
        if new
            .get(next)
            .is_some_and(|kept| kept.tokens == member.tokens)
        {
            next += 1;
            continue;
        }
        if !ts_property_signature_is_optional(member.node)
            || new_names.contains(member.name.as_str())
        {
            return None;
        }
        removed.insert(member.name.clone());
    }
    (next == new.len() && !removed.is_empty()).then_some(removed)
}

/// 引数のパターンが、消したキーの束縛を除いて旧新でトークン一致するか。識別子の引数は
/// そのまま一致すること。rest・ネスト・消したキーの束縛が残る形は不成立。
fn pattern_drops_only(
    old_pattern: Node<'_>,
    old_source: &[u8],
    new_pattern: Node<'_>,
    new_source: &[u8],
    removed: &HashSet<String>,
) -> bool {
    match (old_pattern.kind(), new_pattern.kind()) {
        ("identifier", "identifier") => {
            node_signature_tokens(old_pattern, old_source).is_some()
                && node_signature_tokens(old_pattern, old_source)
                    == node_signature_tokens(new_pattern, new_source)
        }
        ("object_pattern", "object_pattern") => {
            let (Some(old), Some(new)) = (
                pattern_extension::entries(old_pattern, old_source),
                pattern_extension::entries(new_pattern, new_source),
            ) else {
                return false;
            };
            let kept: Vec<_> = old
                .iter()
                .filter(|entry| !removed.contains(&entry.key))
                .collect();
            kept.len() == new.len()
                && kept.iter().zip(&new).all(|(old, new)| {
                    node_signature_tokens(old.node, old_source).is_some_and(|tokens| {
                        Some(tokens) == node_signature_tokens(new.node, new_source)
                    })
                })
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::parser;
    use crate::language::LangId;

    fn removal(old: &str, new: &str, lang: LangId) -> Option<(BTreeSet<String>, bool)> {
        let old_tree = parser::parse_source(old.as_bytes(), lang).unwrap();
        let new_tree = parser::parse_source(new.as_bytes(), lang).unwrap();
        props_removal(
            old_tree.root_node(),
            old.as_bytes(),
            new_tree.root_node(),
            new.as_bytes(),
            "Panel",
        )
        .map(|removal| {
            (
                removal.removed.into_iter().collect(),
                removal.explicit_return,
            )
        })
    }

    const OLD: &str = "export function Panel({ label, legacy, onDone }: { label: string; legacy?: { id: string }; onDone: () => void }) { return <b>{label}</b>; }";
    const NEW: &str = "export function Panel({ label, onDone }: { label: string; onDone: () => void }) { return <b>{label}</b>; }";

    #[test]
    fn optional_props_removal_accepts_only_optional_property_removal() {
        let accepted = |old: &str, new: &str| {
            let result = removal(old, new, LangId::Tsx);
            assert!(result.is_some(), "{old} -> {new}");
            result.unwrap()
        };
        assert_eq!(accepted(OLD, NEW).0, BTreeSet::from(["legacy".to_owned()]));
        // 別名・既定値つきの束縛も、消したキーの分だけ消す。
        accepted(&OLD.replace("legacy,", "legacy: old,"), NEW);
        accepted(&OLD.replace("legacy,", "legacy = { id: 'x' },"), NEW);
        // 分割代入していないキーの削除と、識別子の引数。
        accepted(&OLD.replace("legacy, ", ""), NEW);
        accepted(
            &OLD.replace("{ label, legacy, onDone }", "props"),
            &NEW.replace("{ label, onDone }", "props"),
        );
        // 複数の削除、JSDoc ごとの削除、明示の戻り値型。
        assert_eq!(
            accepted(
                &OLD.replace(
                    "legacy?: { id: string };",
                    "legacy?: { id: string }; tone?: string;"
                ),
                NEW
            )
            .0,
            BTreeSet::from(["legacy".to_owned(), "tone".to_owned()])
        );
        accepted(&OLD.replace("legacy?:", "/** old */ legacy?:"), NEW);
        assert!(
            accepted(
                &OLD.replace(") {", "): JSX.Element {"),
                &NEW.replace(") {", "): JSX.Element {"),
            )
            .1
        );
        // TS でも同じ (直接呼び出しの関数)。
        assert!(
            removal(
                "export function Panel(options: { a: number; legacy?: string }): number { return options.a; }",
                "export function Panel(options: { a: number }): number { return options.a; }",
                LangId::Typescript,
            )
            .is_some()
        );
    }

    #[test]
    fn optional_props_removal_rejects_other_contract_changes() {
        for (old, new) in [
            // 必須の削除・残りの変更・追加・並べ替え
            (OLD.replace("legacy?:", "legacy:"), NEW.to_owned()),
            (
                OLD.to_owned(),
                NEW.replace("label: string", "label: number"),
            ),
            (
                OLD.to_owned(),
                NEW.replace("label: string;", "label: string; extra?: string;"),
            ),
            (
                OLD.to_owned(),
                NEW.replace(
                    "label: string; onDone: () => void",
                    "onDone: () => void; label: string",
                ),
            ),
            (OLD.to_owned(), OLD.to_owned()),
            // 戻り値・async・型パラメータ・export の形
            (OLD.to_owned(), NEW.replace(") {", "): JSX.Element {")),
            (OLD.replace(") {", "): JSX.Element {"), NEW.to_owned()),
            (
                OLD.to_owned(),
                NEW.replace("export function", "export async function"),
            ),
            (
                OLD.replace("Panel(", "Panel<T>("),
                NEW.replace("Panel(", "Panel<T>("),
            ),
            (
                OLD.replace("export function", "export default function"),
                NEW.replace("export function", "export default function"),
            ),
            (
                OLD.replace("export function", "function"),
                NEW.replace("export function", "function"),
            ),
            // 同名の別の宣言・overload
            (
                format!("{OLD} type Panel = 1;"),
                format!("{NEW} type Panel = 1;"),
            ),
            (
                format!("export function Panel(props: {{ label: string }}): null; {OLD}"),
                format!("export function Panel(props: {{ label: string }}): null; {NEW}"),
            ),
            // 引数の形
            (
                OLD.replace(
                    "onDone: () => void }) {",
                    "onDone: () => void } = { label: '', onDone() {} }) {",
                ),
                NEW.replace(
                    "onDone: () => void }) {",
                    "onDone: () => void } = { label: '', onDone() {} }) {",
                ),
            ),
            (
                OLD.replace("{ label, legacy, onDone }", "{ label, legacy, ...rest }"),
                NEW.replace("{ label, onDone }", "{ label, ...rest }"),
            ),
            (
                OLD.replace(
                    "{ label: string; legacy?: { id: string }; onDone: () => void }",
                    "{ label: string; legacy?: { id: string }; onDone: () => void } | undefined",
                ),
                NEW.replace(
                    "{ label: string; onDone: () => void }",
                    "{ label: string; onDone: () => void } | undefined",
                ),
            ),
            (
                OLD.replace(") {", ", extra: number) {"),
                NEW.replace(") {", ", extra: number) {"),
            ),
            (
                OLD.to_owned(),
                NEW.replace("{ label, onDone }", "{ label, legacy, onDone }"),
            ),
            (
                OLD.to_owned(),
                NEW.replace("{ label, onDone }", "{ onDone, label }"),
            ),
            (
                OLD.to_owned(),
                NEW.replace("{ label, onDone }", "{ label: text, onDone }"),
            ),
            (
                OLD.replace("onDone: () => void }", "onDone(): void }"),
                NEW.replace("onDone: () => void }", "onDone(): void }"),
            ),
            (
                OLD.replace(
                    "onDone: () => void }",
                    "onDone: () => void; [key: string]: unknown }",
                ),
                NEW.replace(
                    "onDone: () => void }",
                    "onDone: () => void; [key: string]: unknown }",
                ),
            ),
            (OLD.replace("legacy?:", "'legacy'?:"), NEW.to_owned()),
            (OLD.to_owned(), format!("{NEW} const broken = ;")),
            // 推論の戻り値の分類が変わる・分類できない
            (
                OLD.to_owned(),
                NEW.replace("return <b>{label}</b>;", "return 'x';"),
            ),
            (
                OLD.replace("return <b>{label}</b>;", "return legacy ? <b /> : null;"),
                NEW.to_owned(),
            ),
            (
                OLD.replace("return <b>{label}</b>;", "return view;"),
                NEW.replace("return <b>{label}</b>;", "return view;"),
            ),
            (
                format!("/** @jsx h */ {OLD}"),
                format!("/** @jsx h */ {NEW}"),
            ),
        ] {
            assert!(removal(&old, &new, LangId::Tsx).is_none(), "{old} -> {new}");
        }
    }

    #[test]
    fn optional_props_removal_node_contracts_exist() {
        for lang in [LangId::Typescript, LangId::Tsx] {
            let grammar = lang.ts_language();
            for query in [
                "(function_declaration parameters: (formal_parameters (required_parameter pattern: (object_pattern) type: (type_annotation (object_type)))))",
                "(function_declaration parameters: (formal_parameters (optional_parameter pattern: (identifier))))",
                "(function_declaration return_type: (type_annotation))",
                "(property_signature name: (property_identifier) type: (type_annotation))",
                "(method_signature)",
                "(call_signature)",
                "(construct_signature)",
                "(index_signature)",
                "(object_pattern (shorthand_property_identifier_pattern))",
                "(object_pattern (object_assignment_pattern left: (shorthand_property_identifier_pattern)))",
                "(object_pattern (pair_pattern key: (property_identifier) value: (identifier)))",
                "(object_pattern (rest_pattern))",
                "(call_expression function: (identifier) arguments: (arguments (object)))",
                "(object (pair key: (property_identifier)))",
                "(object (shorthand_property_identifier))",
                "(object (spread_element))",
                "(object (method_definition))",
                "(pair key: (computed_property_name))",
                "(internal_module name: (identifier))",
                "(internal_module name: (nested_identifier))",
                "(module name: (string))",
                "(ambient_declaration)",
                "(comment)",
            ] {
                tree_sitter::Query::new(&grammar, query)
                    .unwrap_or_else(|error| panic!("{lang:?}: {query}: {error}"));
            }
        }
        for query in [
            "(jsx_self_closing_element name: (identifier) attribute: (jsx_attribute (property_identifier)))",
            "(jsx_opening_element name: (identifier) attribute: (jsx_expression (spread_element)))",
            "(jsx_attribute (jsx_namespace_name))",
            "(jsx_element open_tag: (jsx_opening_element) close_tag: (jsx_closing_element name: (identifier)))",
        ] {
            tree_sitter::Query::new(&LangId::Tsx.ts_language(), query)
                .unwrap_or_else(|error| panic!("{query}: {error}"));
        }
    }
}
