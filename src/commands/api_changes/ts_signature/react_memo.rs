//! React の `memo()` で包んだ関数部品の互換の証明 (Issue #266)。
//!
//! `export const Tag = memo(function Tag(props) { … })` (arrow 関数も) の公開型は、`react` の型
//! 定義の `memo` が包んだ関数の型から作る (`NamedExoticComponent<P>` か `MemoExoticComponent<T>`)。
//! 包まない関数部品と同じ証明を内側の関数に委ねるのは、次をすべて確かめたときだけにする。
//!
//! - 変更の前後とも、型注釈の無い一意なトップレベルの const の初期化子 (括弧は透過する) が、
//!   `react` から値として named import した `memo` (`memo as m` も) に直接の関数を 1 つだけ渡す
//!   呼び出しで、内側の関数の種類と、その前後のトークン (`export const Tag = memo(` と `);`) が
//!   同じ。比較関数・型引数は、包んだ関数との型の適合を証明できないので受け付けない
//! - その名前の module の束縛が import の 1 つだけで、どこでも書き換えていない
//! - `react` が registry の React に解決され (import するファイルの祖先の package.json の registry の
//!   依存。ワークスペースのパッケージの package.json にだけ書いたものも、Issue #280)、設定・
//!   依存・ソースが React と JSX の型を差し替えていない (解析するディレクトリの全体の監査)
//! - diff が `react` の解決と型の環境を変えていない。設定・package.json・ロックファイルを変えず、
//!   変えたソースの変更前も同じ監査を通り、削除・改名したファイルが `react` の解決先の候補でない
//!
//! 包まない関数部品と同じく、利用側 (余分なプロパティを持つ値の spread、`keyof` の利用など) は
//! 検査しない。

use std::collections::HashSet;

use tree_sitter::Node;

use super::super::signature_tokens::{SigTokens, signature_tokens_in_range};
use super::TS_ONLY_LANGS;
use super::callable::{ts_site_languages, unwrap_parentheses};
use super::repository_audit::ReactEnvironment;
use crate::commands::api_changes::js_const::{is_module_binding, unique_top_level_const};
use crate::commands::api_changes::js_ts_shadow::{pattern_binds_name, visit_import_bindings};
use crate::commands::api_changes::source_pair::{CompatibleModSite, SignatureSourceCache};
use crate::commands::api_changes::ts_alias_resolution::{AliasResolution, TsAliasResolver};
use crate::commands::api_changes::ts_const_arg::collect_bindings_named;
use crate::commands::dead_code_default_liveness::static_specifier;
use crate::engine::tree_walk::{Children, any_node};
use crate::git_support::normalize_workspace_separators;
use crate::models::impact::DiffFile;

/// `value` (const の初期化子から括弧を外したもの) が、`react` から値として import した `memo` に
/// 直接の関数 (function 式か arrow 関数) を 1 つだけ渡す呼び出しなら、その関数。型引数・比較関数・
/// optional call・spread を含む呼び出しは None。
pub(super) fn react_memo_function<'tree>(
    root: Node<'tree>,
    source: &[u8],
    value: Node<'tree>,
) -> Option<Node<'tree>> {
    if value.kind() != "call_expression" {
        return None;
    }
    let callee = value.child_by_field_name("function")?;
    let arguments = value.child_by_field_name("arguments")?;
    if callee.kind() != "identifier" || arguments.kind() != "arguments" {
        return None;
    }
    let mut cursor = value.walk();
    if value.children(&mut cursor).any(|child| {
        child.id() != callee.id() && child.id() != arguments.id() && child.kind() != "comment"
    }) {
        return None;
    }
    let mut cursor = arguments.walk();
    let values: Vec<_> = arguments
        .named_children(&mut cursor)
        .filter(|child| child.kind() != "comment")
        .collect();
    let [argument] = values.as_slice() else {
        return None;
    };
    let function = unwrap_parentheses(*argument)?;
    if !matches!(function.kind(), "arrow_function" | "function_expression") {
        return None;
    }
    binds_react_memo(root, source, callee.utf8_text(source).ok()?).then_some(function)
}

/// `local` の module の束縛が `react` からの値の named import の `memo` だけで、どこでも書き換えて
/// いないか。関数の中の同名の束縛は、module の直下の呼び出しに届かないので数えない。
fn binds_react_memo(root: Node<'_>, source: &[u8], local: &str) -> bool {
    let bindings = collect_bindings_named(root, source, local);
    // TypeScript は import への書き込みと同じ名前の `var` を拒むが、構文としては書けるので拒む。
    // 代入 (`for` の左辺を含む) は、関数の中の同名の束縛へのものか決めずに、どのスコープでも
    // 拒む。`var` は、巻き上げで関数と class の static block の外 (module) に届くときに拒む。
    let writes = bindings.iter().any(|node| match node.kind() {
        "assignment_expression" | "augmented_assignment_expression" => true,
        "for_in_statement" => node
            .child_by_field_name("kind")
            .is_none_or(|kind| kind.kind() == "var" && var_reaches_module(*node)),
        "variable_declarator" => {
            node.parent()
                .is_some_and(|parent| parent.kind() == "variable_declaration")
                && var_reaches_module(*node)
        }
        _ => false,
    }) || any_node(root, Children::Named, |node| {
        node.kind() == "update_expression"
            && node
                .child_by_field_name("argument")
                .is_some_and(|argument| pattern_binds_name(argument, source, local))
    });
    if writes {
        return false;
    }
    let module: Vec<_> = bindings
        .into_iter()
        .filter(|node| is_module_binding(*node))
        .collect();
    let [import] = module.as_slice() else {
        return false;
    };
    import.kind() == "import_statement" && imports_react_memo(*import, source, local)
}

/// `var` の宣言 `node` の巻き上げ先が module か (関数と class の static block の中でない)。
/// 対象の名前の束縛だけに使うので、祖先を辿る回数は限られる。
fn var_reaches_module(node: Node<'_>) -> bool {
    let mut current = node.parent();
    while let Some(ancestor) = current {
        if matches!(
            ancestor.kind(),
            "function_declaration"
                | "function_expression"
                | "arrow_function"
                | "method_definition"
                | "generator_function_declaration"
                | "generator_function"
                | "class_static_block"
        ) {
            return false;
        }
        current = ancestor.parent();
    }
    true
}

/// `import` が `react` から `memo` を値として named import し、`local` をその 1 つだけで束縛する
/// か。`import type`・`{ type memo }`・文字列の名前の import は受け付けない。
fn imports_react_memo(import: Node<'_>, source: &[u8], local: &str) -> bool {
    let mut cursor = import.walk();
    if import.children(&mut cursor).any(type_only_keyword) {
        return false;
    }
    if import
        .child_by_field_name("source")
        .and_then(|specifier| static_specifier(specifier, source))
        != Some("react")
    {
        return false;
    }
    let mut count = 0usize;
    visit_import_bindings(import, source, |name| {
        count += usize::from(name == local);
        false
    });
    count == 1
        && any_node(import, Children::Named, |node| {
            node.kind() == "import_specifier" && specifier_imports_memo(node, source, local)
        })
}

/// 値を束縛しない import の印 (`import type`・`import typeof` と、specifier の `type`)。
fn type_only_keyword(node: Node<'_>) -> bool {
    !node.is_named() && matches!(node.kind(), "type" | "typeof")
}

fn specifier_imports_memo(specifier: Node<'_>, source: &[u8], local: &str) -> bool {
    let mut cursor = specifier.walk();
    if specifier.children(&mut cursor).any(type_only_keyword) {
        return false;
    }
    let Some(name) = specifier.child_by_field_name("name") else {
        return false;
    };
    let bound = specifier.child_by_field_name("alias").unwrap_or(name);
    name.kind() == "identifier"
        && bound.kind() == "identifier"
        && name.utf8_text(source).ok() == Some("memo")
        && bound.utf8_text(source).ok() == Some(local)
}

/// 変更の前後で比べる、`memo` で包んだ const の形。
struct MemoCallable<'tree> {
    function: Node<'tree>,
    /// 宣言の始まりから内側の関数の前まで (`export const Tag = memo(`)。
    prefix: SigTokens,
    /// 内側の関数の後ろから宣言の終わりまで (`);`)。
    suffix: SigTokens,
}

/// 型注釈の無い一意なトップレベルの const `name` が、`react` の `memo` で直接の関数を包んだもの
/// なら、その形。型注釈は `memo` の結果の型との適合を要るので証明しない。
fn memo_callable<'tree>(
    root: Node<'tree>,
    source: &[u8],
    name: &str,
) -> Option<MemoCallable<'tree>> {
    let (declarator, statement) = unique_top_level_const(root, source, name)?;
    if declarator.child_by_field_name("type").is_some() {
        return None;
    }
    let value = unwrap_parentheses(declarator.child_by_field_name("value")?)?;
    let function = react_memo_function(root, source, value)?;
    Some(MemoCallable {
        function,
        prefix: signature_tokens_in_range(
            statement,
            source,
            statement.start_byte(),
            function.start_byte(),
        )?,
        suffix: signature_tokens_in_range(
            statement,
            source,
            function.end_byte(),
            statement.end_byte(),
        )?,
    })
}

/// 変更の前後とも `react` の `memo` で直接の関数を同じ形で包んだ const なら、内側の関数の組を
/// `check` に渡して、その結果を返す。`react` を信頼できなければ None。省略可能な props の追加
/// (`optional_object_props`) だけに使う。
pub(super) fn with_resolved_react_memo_pair<T>(
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
    cache: &mut ReactMemoCache,
    diff_files: &[DiffFile],
    check: impl FnOnce(Node<'_>, &[u8], Node<'_>, &[u8]) -> Option<T>,
) -> Option<T> {
    // 改名・移動したファイルは、変更の前後で `react` の解決の起点が変わり得る。
    if !matches!(site.kind, "variable" | "constant")
        || site.name.is_empty()
        || site.name.contains('.')
        || site.old_path != site.new_path
    {
        return None;
    }
    site.lang_in(TS_ONLY_LANGS)?;
    let (old_lang, new_lang) = ts_site_languages(site)?;
    let result = {
        let src = sources.get(site)?;
        let old_tree = crate::engine::parser::parse_source(&src.old, old_lang).ok()?;
        let new_tree = crate::engine::parser::parse_source(&src.new, new_lang).ok()?;
        let old = memo_callable(old_tree.root_node(), &src.old, site.name)?;
        let new = memo_callable(new_tree.root_node(), &src.new, site.name)?;
        if old.function.kind() != new.function.kind()
            || old.prefix != new.prefix
            || old.suffix != new.suffix
        {
            return None;
        }
        check(old.function, &src.old, new.function, &src.new)?
    };
    cache
        .trusts_react(site, sources, diff_files)
        .then_some(result)
}

/// 1 回の API 変更の検出に閉じる。どれも最初に要ったときに一度だけ求める。
#[derive(Default)]
pub(crate) struct ReactMemoCache {
    /// diff が設定・依存の指定・ロックファイルの名前のファイルを変えていない。
    names_kept: Option<bool>,
    resolver: Option<TsAliasResolver>,
    /// 解析するディレクトリの全体の監査。内の None は、差し替えがあるか監査できない。
    environment: Option<Option<ReactEnvironment>>,
    /// diff が、読んだ設定と、変えたソースの変更前の中身でも、環境を変えていない。
    environment_kept: Option<bool>,
}

impl ReactMemoCache {
    /// `site` のファイルの `react` が、変更の前後とも registry の React を指し、React と JSX の型を
    /// どこも差し替えていないか。安い検査から順に確かめる。
    fn trusts_react(
        &mut self,
        site: &CompatibleModSite<'_>,
        sources: &SignatureSourceCache<'_>,
        diff_files: &[DiffFile],
    ) -> bool {
        let Self {
            names_kept,
            resolver,
            environment,
            environment_kept,
        } = self;
        if !*names_kept.get_or_insert_with(|| {
            changed_paths(diff_files).all(|path| !ReactEnvironment::names_environment(&path))
        }) {
            return false;
        }
        let resolver = resolver
            .get_or_insert_with(|| TsAliasResolver::load(site.dir, removed_files(diff_files)));
        if !matches!(
            resolver.resolve(site.new_path, "react"),
            AliasResolution::External
        ) {
            return false;
        }
        let Some(environment) = environment
            .get_or_insert_with(|| ReactEnvironment::load(site.dir))
            .as_ref()
        else {
            return false;
        };
        *environment_kept
            .get_or_insert_with(|| keeps_environment(environment, site, sources, diff_files))
    }
}

/// diff の変更前と変更後のパス (`dir` 相対、`/` 区切り)。
fn changed_paths(diff_files: &[DiffFile]) -> impl Iterator<Item = String> + '_ {
    diff_files
        .iter()
        .flat_map(|file| [&file.old_path, &file.new_path])
        .filter(|path| *path != "/dev/null")
        .map(|path| normalize_workspace_separators(path))
}

/// 削除・改名で変更後に無くなったパス。変更前は `react` の解決先だったかもしれない。
fn removed_files(diff_files: &[DiffFile]) -> HashSet<String> {
    diff_files
        .iter()
        .filter(|file| file.old_path != "/dev/null" && file.old_path != file.new_path)
        .map(|file| normalize_workspace_separators(&file.old_path))
        .collect()
}

/// 変更前の環境が、いまの環境と同じく `react` を差し替えていなかったか。読んだ設定 (`extends` の先
/// など) を変えず、変えたソースの変更前の中身も監査を通る。
fn keeps_environment(
    environment: &ReactEnvironment,
    site: &CompatibleModSite<'_>,
    sources: &SignatureSourceCache<'_>,
    diff_files: &[DiffFile],
) -> bool {
    if changed_paths(diff_files).any(|path| environment.is_environment_path(&path)) {
        return false;
    }
    diff_files
        .iter()
        .filter(|file| file.old_path != "/dev/null")
        .all(|file| {
            let old_path = normalize_workspace_separators(&file.old_path);
            environment.keeps_standard(&old_path, || sources.read_base(site, &old_path))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{engine::parser, language::LangId};

    fn memo_function(source: &str) -> Option<String> {
        let tree = parser::parse_source(source.as_bytes(), LangId::Tsx).unwrap();
        let root = tree.root_node();
        let callable = memo_callable(root, source.as_bytes(), "Tag")?;
        Some(
            callable
                .function
                .utf8_text(source.as_bytes())
                .unwrap()
                .to_owned(),
        )
    }

    #[test]
    fn memo_callable_accepts_only_a_direct_function_passed_to_react_memo() {
        for source in [
            "import { memo } from 'react';\nexport const Tag = memo(function Tag(p: { a: string }) { return null; });",
            "import { memo } from \"react\";\nexport const Tag = memo((p: { a: string }) => null);",
            "import { memo as m } from 'react';\nexport const Tag = m(((p: { a: string }) => null));",
            "import React, { memo, type FC } from 'react';\nexport const Tag = (memo(/* c */ (p: { a: string }) => null,));",
            "import { memo } from 'react';\nconst Tag = memo((p: { a: string }) => null);\nexport { Tag };",
            // 関数の中の同名の束縛は、module の直下の呼び出しに届かない。
            "import { memo } from 'react';\nexport const Tag = memo((p: { a: string }) => null);\nfunction other(memo: number) { const inner = (memo: string) => memo; return memo; }",
            // 関数と class の static block の中の `var` は、そこから外へ巻き上がらない。
            "import { memo } from 'react';\nfunction helper() { var memo = 0; return memo; }\nexport const Tag = memo((p: { a: string }) => null);",
            "import { memo } from 'react';\nclass Holder { static { var memo = 1; } }\nexport const Tag = memo((p: { a: string }) => null);",
            "import { memo } from 'react';\nconst run = () => { for (var memo of [1]) {} };\nexport const Tag = memo((p: { a: string }) => null);",
            // import の再 export は束縛を変えない。
            "import { memo } from 'react';\nexport { memo };\nexport const Tag = memo((p: { a: string }) => null);",
        ] {
            assert_eq!(
                memo_function(source)
                    .as_deref()
                    .map(|text| text.contains("=>") || text.starts_with("function")),
                Some(true),
                "{source}"
            );
        }
        for source in [
            // 比較関数・型引数・optional call・spread・関数でない引数
            "import { memo } from 'react';\nexport const Tag = memo((p: { a: string }) => null, areEqual);",
            "import { memo } from 'react';\nexport const Tag = memo<Props>((p: Props) => null);",
            "import { memo } from 'react';\nexport const Tag = memo?.((p: { a: string }) => null);",
            "import { memo } from 'react';\nexport const Tag = memo(...parts);",
            "import { memo } from 'react';\nexport const Tag = memo(Inner);",
            "import { memo } from 'react';\nexport const Tag = memo(function* (p: { a: string }) {});",
            "import { memo } from 'react';\nexport const Tag = memo(forwardRef((p: { a: string }) => null));",
            "import { memo } from 'react';\nexport const Tag = memo((p: { a: string }) => null) as Fc;",
            "import { memo } from 'react';\nexport const Tag: Fc = memo((p: { a: string }) => null);",
            "import { memo } from 'react';\nexport let Tag = memo((p: { a: string }) => null);",
            "import * as React from 'react';\nexport const Tag = React.memo((p: { a: string }) => null);",
            // react 以外・型だけの import・default import・文字列の名前
            "import { memo } from 'preact/compat';\nexport const Tag = memo((p: { a: string }) => null);",
            "import { memo } from './react';\nexport const Tag = memo((p: { a: string }) => null);",
            "import type { memo } from 'react';\nexport const Tag = memo((p: { a: string }) => null);",
            "import { type memo } from 'react';\nexport const Tag = memo((p: { a: string }) => null);",
            "import memo from 'react';\nexport const Tag = memo((p: { a: string }) => null);",
            "import { useMemo as memo } from 'react';\nexport const Tag = memo((p: { a: string }) => null);",
            "import { \"memo\" as memo } from 'react';\nexport const Tag = memo((p: { a: string }) => null);",
            "export const Tag = memo((p: { a: string }) => null);",
            // 同じ名前の別の束縛・書き込み
            "import { memo } from 'react';\nimport { memo } from 'other';\nexport const Tag = memo((p: { a: string }) => null);",
            "import { memo } from 'react';\nfunction memo() {}\nexport const Tag = memo((p: { a: string }) => null);",
            "import { memo, memo as memo } from 'react';\nexport const Tag = memo((p: { a: string }) => null);",
            "import { memo } from 'react';\nmemo = wrap;\nexport const Tag = memo((p: { a: string }) => null);",
            "import { memo } from 'react';\nfunction patch() { memo = wrap; }\nexport const Tag = memo((p: { a: string }) => null);",
            "import { memo } from 'react';\nfunction patch() { memo++; }\nexport const Tag = memo((p: { a: string }) => null);",
            "import { memo } from 'react';\nfunction patch(xs) { for (memo of xs) {} }\nexport const Tag = memo((p: { a: string }) => null);",
            "import { memo } from 'react';\n{ var memo = wrap; }\nexport const Tag = memo((p: { a: string }) => null);",
            "import { memo } from 'react';\n{\n  for (var memo of [wrap]) {}\n}\nexport const Tag = memo((p: { a: string }) => null);",
            "import { memo } from 'react';\nfor (var memo in registry) {}\nexport const Tag = memo((p: { a: string }) => null);",
            "import { memo } from 'react';\nfunction patch() { [memo] = [wrap]; }\nexport const Tag = memo((p: { a: string }) => null);",
            "import { memo } from 'react';\nimport memo = require('react');\nexport const Tag = memo((p: { a: string }) => null);",
            "import { memo } from 'react';\ndeclare function memo<T>(value: T): T;\nexport const Tag = memo((p: { a: string }) => null);",
            "import { memo } from 'react';\nnamespace memo { export const extra = 1; }\nexport const Tag = memo((p: { a: string }) => null);",
            // callee が識別子でない呼び出し
            "import { memo } from 'react';\nexport const Tag = memo!((p: { a: string }) => null);",
            "import { memo } from 'react';\nexport const Tag = (memo)((p: { a: string }) => null);",
            "import { memo } from 'react';\nexport const Tag = (0, memo)((p: { a: string }) => null);",
        ] {
            assert_eq!(memo_function(source), None, "{source}");
        }
    }

    #[test]
    fn memo_callable_keeps_the_tokens_around_the_function() {
        let old = memo_callable_tokens(
            "import { memo } from 'react';\nexport const Tag = memo(function Tag(p: { a: string }) { return null; });",
        );
        for (new, same) in [
            (
                "import { memo } from 'react';\nexport const Tag = memo(function Tag(p: { a: string; b?: string }) { return null; });",
                true,
            ),
            (
                "import { memo } from 'react';\nexport const Tag = memo((function Tag(p: { a: string; b?: string }) { return null; }));",
                false,
            ),
            (
                "import { memo as m } from 'react';\nexport const Tag = m(function Tag(p: { a: string; b?: string }) { return null; });",
                false,
            ),
            (
                "import { memo } from 'react';\nexport const Tag = memo(function Tag(p: { a: string; b?: string }) { return null; }) /* c */;",
                false,
            ),
        ] {
            assert_eq!(memo_callable_tokens(new) == old, same, "{new}");
        }
    }

    fn memo_callable_tokens(source: &str) -> (SigTokens, SigTokens) {
        let tree = parser::parse_source(source.as_bytes(), LangId::Tsx).unwrap();
        let callable = memo_callable(tree.root_node(), source.as_bytes(), "Tag").unwrap();
        (callable.prefix, callable.suffix)
    }
}
