//! 明示した enum と crate 起点の直接 import から、variant の所有元だけを確認する。

use super::*;
use crate::engine::rust_use_tree::{RustPubUseEdge, UsePathOrigin, expand_rust_use_tree_edges_ast};

/// 配置を変え得るディレクトリの一覧と、`literal_includes` で受け入れた `include!` があったか、
/// 木の中に `macro_rules!` の定義 (マクロの本体・引数の中の `macro_rules` も) か、走査で読み切れない
/// 入力 (先頭行の `#! [..]` の内側の属性・root の外を指す path の依存) があったか。
/// `literal_includes` が偽なら `include!` を 1 つでも見つけた時点で `None`。
pub(super) fn static_layout(
    root: &Path,
    project: &RustPathOwnerProjectProof,
    literal_includes: bool,
) -> Option<(Vec<PathBuf>, bool, bool)> {
    if !project.layout_cargo_settings_known() {
        return None;
    }
    let root = root.canonicalize().ok()?;
    if root
        .ancestors()
        .skip(1)
        .any(|path| path.join("Cargo.toml").exists() || path.join(".git").exists())
    {
        return None;
    }
    // snapshot が書き出さなかった要素も、下の走査が実際の checkout の symlink に当てる規則で扱う
    // (通常のファイルを指し、`.rs` でも `Cargo.toml` でもない symlink だけを許す)。root の直下の
    // `.git`・`target` のディレクトリの中は読まない (Issue #376)。
    let omitted_allowed = |path: &Path, kind: &crate::git_support::OmittedKind| {
        if super::under_root_git_or_target(path)
            && (path.components().count() > 1
                || matches!(kind, crate::git_support::OmittedKind::Gitlink))
        {
            return true;
        }
        matches!(
            kind,
            crate::git_support::OmittedKind::Symlink(crate::git_support::LinkTarget::RegularFile)
        ) && !path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("rs"))
            && !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case("Cargo.toml"))
    };
    if !crate::git_support::tree_view(&root)
        .omitted()
        .is_some_and(|omitted| {
            omitted
                .iter()
                .all(|(path, kind)| omitted_allowed(path, kind))
        })
    {
        return None;
    }
    let mut pending = vec![root.clone()];
    let mut relocated = Vec::new();
    let mut included = false;
    let mut candidates = Vec::new();
    let mut candidate_bytes = 0usize;
    // `macro_rules!` の定義 (名前ごとに、本体に `mod` の綴りがあるかと、本体で呼ぶマクロの名前。
    // 呼び出しが子の `mod` を作り得るかを、走査の後で委譲まで含めて決める) と、走査の後で子の
    // `mod` を確かめる `#[path]` の先。
    let mut macro_definitions: HashMap<String, MacroDefinition> = HashMap::new();
    // `use children as mount;` の別名 (別名, 元の名前)。
    let mut macro_aliases: Vec<(String, String)> = Vec::new();
    let mut path_targets: Vec<PathBuf> = Vec::new();
    // `include!` で取り込んだファイル (`.rs` でなければ走査が読まないので、別名を後で集める)。
    let mut included_files: Vec<PathBuf> = Vec::new();
    // 属性の角括弧の前でない `#` (`mount!(#, path)`) の、(マクロの定義の中か, 渡す先のマクロの名前)。
    let mut bare_hashes: Vec<(bool, Option<String>)> = Vec::new();
    let mut assembled = AttributeInputs::default();
    // マクロの本体か引数の中に `macro_rules` がある (マクロがマクロを定義する。`install!(mount)` が
    // `macro_rules! mount { .. }` を作る、`factory!(macro_rules, install)`)。作られる名前は分からない。
    let mut generates_macros = false;
    // マクロの本体か引数の token tree に `use` がある (マクロが `use crate::{children as mount};` を作り、
    // 子の `mod` を作るマクロに分からない名前を付け得る)。
    let mut aliases_in_macros = false;
    // token tree の中のフィールドに見える `include` (`loader.include`) の、(マクロの定義の中か,
    // 渡す先のマクロの名前)。
    let mut include_fields: Vec<(bool, Option<String>)> = Vec::new();
    // マクロの本体か引数の token tree に現れる、信じる木の外のマクロの名前 ([`TRUSTED_MACROS`])。
    let mut array_macros = HashSet::new();
    let mut tree_words: HashSet<String> = HashSet::new();
    // 走査で読み切れない入力 (先頭行の `#! [..]` の内側の属性・root の外を指す path の依存)。
    // `macro_rules!` の定義と同じく、配置は決めても、字句を組み立てる経路が無いとは言わない。
    let mut open = false;
    let mut external_macros = false;
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory).ok()? {
            let entry = entry.ok()?;
            let kind = entry.file_type().ok()?;
            let file = entry.path();
            let entry_name = entry.file_name();
            if kind.is_symlink() {
                if std::fs::metadata(&file).ok()?.is_file()
                    && !file
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("rs"))
                    && !entry_name
                        .to_str()
                        .is_some_and(|name| name.eq_ignore_ascii_case("Cargo.toml"))
                {
                    continue;
                }
                return None;
            }
            if kind.is_dir() {
                if directory != root || !matches!(entry_name.to_str(), Some(".git" | "target")) {
                    pending.push(file);
                }
            } else if kind.is_file()
                && entry_name
                    .to_str()
                    .is_some_and(|name| name.eq_ignore_ascii_case("Cargo.toml"))
            {
                let package = directory.strip_prefix(&root).ok()?;
                let manifest = project.alias_cache.manifest(&root, package)?;
                // 木の中の proc-macro のクレートは、どのマクロの呼び出し・derive・属性からも
                // (`#[path]` で読んだファイルの外の普通のモジュールでも) `mod calls;`・`include!(..)`・
                // `#[path]` を返し得る (作るマクロの名前も取り込む先も分からない) ので、配置を決めない。
                external_macros |= !super::dependency_tables(&manifest)?.is_empty();
                if manifest_is_proc_macro(&manifest) {
                    return None;
                }
                // root の直下の `target`・`.git` を指す path の依存は、そこの package を
                // workspace の member にする (走査はその中身を読まない)。
                if super::path_dependencies_reach_unscanned(&root, &manifest, package)
                    != Some(false)
                {
                    return None;
                }
                // root の外を指す path の依存 (`probe = { path = "../../probe" }`) は、走査が読まない
                // package を依存にする (その package が木のファイルを `include!`・`#[path]` で
                // 取り込み得る)。大文字小文字を区別して比べる。root の外の workspace に属す
                // package (`package.workspace = "../../outer"`) は、継承する依存
                // (`probe.workspace = true`) の先を読めない。
                open |= super::path_dependencies_leave_root(&root, &manifest, package)?
                    || super::package_workspace_outside_root(&root, &manifest, package);
                let targets = RustTargets::from_manifest(&manifest, package)?;
                if !targets.roots_complete {
                    return None;
                }
                for target in targets.explicit_roots() {
                    if target
                        .extension()
                        .is_none_or(|extension| !extension.eq_ignore_ascii_case("rs"))
                    {
                        return None;
                    }
                    let target = root.join(target).canonicalize().ok()?;
                    // root の直下の `.git`・`target` の下の target は、走査が中身を読まない。
                    if !target.starts_with(&root)
                        || super::under_root_git_or_target(target.strip_prefix(&root).ok()?)
                    {
                        return None;
                    }
                    relocated.push(target.parent()?.to_owned());
                }
            } else if kind.is_file()
                && file
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("rs"))
            {
                let source = read_input_file(&file)?;
                // 先頭行の `#! [allow(dead_code)]` は rustc では内側の属性だが、同梱の文法は行全体を
                // shebang にし、同じ行の `macro_rules!`・`#[path]` を読まない。
                open |= hidden_inner_attribute(&source);
                // マクロの定義は、別のファイルの呼び出し (`attach!(path = "...")`) で `#[path]` を
                // 作り得るので、`path` の綴りが無くても読む。マクロへ渡す裸の `#` (`wrap!(#)`) も、
                // 別のファイルの `path` と組み合わさり得るので読む。`#!` (同梱の文法が shebang に
                // して隠す `mod calls;`) と、nightly の `macro` 項目 (`pub macro mount() { .. }`。
                // `-Zcrate-attr=feature(decl_macro)` なら `.rs` に機能の綴りが無い。同梱の文法は
                // 読めないので構文エラーになり、配置を決めない) の綴りも読む。
                if memchr::memmem::find(&source, b"path").is_none()
                    && memchr::memmem::find(&source, b"include").is_none()
                    && memchr::memmem::find(&source, b"macro").is_none()
                    && memchr::memmem::find(&source, b"#!").is_none()
                    && !may_have_bare_hash(&source)
                    && !(memchr::memchr(b'!', &source).is_some()
                        && (memchr::memchr(b'#', &source).is_some()
                            || [b"mod".as_slice(), b"///", b"//!", b"/**", b"/*!"]
                                .iter()
                                .any(|word| memchr::memmem::find(&source, word).is_some())))
                {
                    continue;
                }
                if !nfc_source(&source) {
                    return None;
                }
                let tree = parser::parse_source(&source, LangId::Rust).ok()?;
                if tree.root_node().has_error() {
                    return None;
                }
                // 先頭のコメントの後の `#! [..]` も、同梱の文法は shebang にする。`.rs` のファイルを
                // 文字列として読む `include_str!` は、`#[doc = ..]` で doctest の本体にできる。
                open |= hidden_shebang(tree.root_node(), &source)
                    || crate::engine::rust_dynamic_source::any_rust_text_include(
                        tree.root_node(),
                        &source,
                        |_| true,
                    );
                let mut offsets = Vec::new();
                let mut macro_scopes = Vec::new();
                let result = crate::engine::tree_walk::walk_preorder(
                    tree.root_node(),
                    crate::engine::tree_walk::Children::All,
                    |node, depth| {
                        assembled.observe(node, depth, &mut macro_scopes, &source);
                        if node.kind() == "macro_definition"
                            && let Some(name) = node
                                .child_by_field_name("name")
                                .and_then(|name| name.utf8_text(&source).ok())
                        {
                            let definition = macro_definitions
                                .entry(name.strip_prefix("r#").unwrap_or(name).to_owned())
                                .or_default();
                            definition.mentions_mod |=
                                node.utf8_text(&source).is_ok_and(mentions_mod);
                            definition.invoked.extend(invoked_macros(node, &source));
                        }
                        if let Some(alias) = use_alias(node, &source) {
                            macro_aliases.push(alias);
                        }
                        if matches!(node.kind(), "macro_invocation" | "use_declaration")
                            && crate::engine::rust_dynamic_source::node_includes_external_source(
                                node, &source,
                            )
                        {
                            if literal_includes
                                && let Some(target) = literal_include(node, &source, &file, &root)
                                && let Some(base) = file
                                    .ancestors()
                                    .skip(1)
                                    .find(|base| target.starts_with(base))
                            {
                                // 取り込み先と元の両方のモジュール配置を保留する。
                                relocated.push(base.to_owned());
                                included_files.push(target);
                                included = true;
                                return std::ops::ControlFlow::Continue(
                                    crate::engine::tree_walk::Walk::Skip,
                                );
                            }
                            return std::ops::ControlFlow::Break(());
                        }
                        if token_list(node) && has_bare_hash(node, &source) {
                            bare_hashes.push(macro_destination(node, &source));
                        }
                        generates_macros |=
                            token_list(node) && token_tree_names_macro_rules(node, &source);
                        aliases_in_macros |= token_list(node) && token_tree_has_use(node);
                        if matches!(node.kind(), "attribute" | "token_tree" | "token_repetition")
                            && crate::engine::rust_dynamic_source::node_includes_external_source(
                                node, &source,
                            )
                        {
                            if node.kind() == "token_tree"
                                && node.child(0).is_some_and(|n| n.kind() == "[")
                                && node.child(1).and_then(|n| name(n, &source)) == Some("path")
                                && node
                                    .child(2)
                                    .is_some_and(|n| matches!(n.kind(), "]" | "." | "token_tree"))
                            {
                                let (in_definition, destination) =
                                    include_destination(node, &source);
                                if !macro_scopes.last().is_some_and(|scope| scope.simple) {
                                    return std::ops::ControlFlow::Break(());
                                }
                                let Some(target) = destination.filter(|_| !in_definition) else {
                                    return std::ops::ControlFlow::Break(());
                                };
                                array_macros.insert(target);
                                return std::ops::ControlFlow::Continue(
                                    crate::engine::tree_walk::Walk::Descend,
                                );
                            }
                            let target = if node.kind() == "attribute" {
                                path_target(node, &source, &file)
                            } else {
                                None
                            };
                            let Some(target) = target.filter(|target| target.starts_with(&root))
                            else {
                                return std::ops::ControlFlow::Break(());
                            };
                            // この走査は root の直下の `.git`・`target` と `.rs` でないファイルの中を
                            // 読まない。そこから別のソースを取り込み得る (`bridge.inc` の
                            // `include!("../b/src/calls.rs")`) なら、配置を決めない。
                            if target
                                .strip_prefix(&root)
                                .ok()
                                .is_none_or(super::under_root_git_or_target)
                                || (target
                                    .extension()
                                    .is_none_or(|extension| !extension.eq_ignore_ascii_case("rs"))
                                    && may_include_sources(&target))
                            {
                                return std::ops::ControlFlow::Break(());
                            }
                            if target.parent().is_none() {
                                return std::ops::ControlFlow::Break(());
                            }
                            relocated.push(target.clone());
                            path_targets.push(target);
                        }
                        if node.kind() == "identifier"
                            && name(node, &source) == Some("include")
                            && has_token_tree_ancestor(node)
                        {
                            if is_field_name_in_token_tree(node, &source) {
                                include_fields.push(include_destination(node, &source));
                            } else {
                                offsets.push(node.start_byte());
                            }
                        }
                        if hidden_include(node, &source) {
                            return std::ops::ControlFlow::Break(());
                        }
                        std::ops::ControlFlow::Continue(crate::engine::tree_walk::Walk::Descend)
                    },
                );
                if result.is_break() {
                    return None;
                }
                if !offsets.is_empty() {
                    candidate_bytes = candidate_bytes.checked_add(source.len())?;
                    if candidate_bytes > 64 * 1024 * 1024 {
                        return None;
                    }
                    candidates.push((file, source, offsets));
                }
            }
        }
    }
    // `#[path]`・`include!` で読む `.rs` でないファイル (`defs.inc`) は走査が読まないので、その中の
    // マクロの定義 (`#[macro_use]` で外へ出る)・別名・裸の `#` もここで集める。
    let extra: Vec<PathBuf> = path_targets
        .iter()
        .chain(&included_files)
        .filter(|path| {
            path.extension()
                .is_none_or(|extension| !extension.eq_ignore_ascii_case("rs"))
        })
        .cloned()
        .collect();
    for file in &extra {
        let source = read_input_file(file)?;
        if !nfc_source(&source) {
            return None;
        }
        let tree = parser::parse_source(&source, LangId::Rust).ok()?;
        if tree.root_node().has_error() {
            return None;
        }
        open |= hidden_inner_attribute(&source)
            || hidden_shebang(tree.root_node(), &source)
            || crate::engine::rust_dynamic_source::any_rust_text_include(
                tree.root_node(),
                &source,
                |_| true,
            );
        let mut macro_scopes = Vec::new();
        let result = crate::engine::tree_walk::walk_preorder(
            tree.root_node(),
            crate::engine::tree_walk::Children::All,
            |node, depth| {
                assembled.observe(node, depth, &mut macro_scopes, &source);
                if node.kind() == "macro_definition"
                    && let Some(name) = node
                        .child_by_field_name("name")
                        .and_then(|name| name.utf8_text(&source).ok())
                {
                    let definition = macro_definitions
                        .entry(name.strip_prefix("r#").unwrap_or(name).to_owned())
                        .or_default();
                    definition.mentions_mod |= node.utf8_text(&source).is_ok_and(mentions_mod);
                    definition.invoked.extend(invoked_macros(node, &source));
                }
                if let Some(alias) = use_alias(node, &source) {
                    macro_aliases.push(alias);
                }
                if token_list(node) && has_bare_hash(node, &source) {
                    bare_hashes.push(macro_destination(node, &source));
                }
                generates_macros |= token_list(node) && token_tree_names_macro_rules(node, &source);
                aliases_in_macros |= token_list(node) && token_tree_has_use(node);
                if token_list(node) {
                    collect_trusted_words(node, &source, &mut tree_words);
                }
                // token tree の `include`: フィールドに見えるものは行き先で決め、ほかは (標準の式の
                // マクロの証明を回さずに) 配置を決めない。
                if node.kind() == "identifier"
                    && name(node, &source) == Some("include")
                    && has_token_tree_ancestor(node)
                {
                    if !is_field_name_in_token_tree(node, &source) {
                        return std::ops::ControlFlow::Break(());
                    }
                    include_fields.push(include_destination(node, &source));
                }
                if hidden_include(node, &source) {
                    return std::ops::ControlFlow::Break(());
                }
                std::ops::ControlFlow::Continue(crate::engine::tree_walk::Walk::Descend)
            },
        );
        if result.is_break() {
            return None;
        }
    }
    // `#[path]` で読んだファイルは名前によらず mod.rs と同じく、子の `mod` を自分のディレクトリから
    // 探す (`bridge.rs` の `mod calls;` は隣の `calls.rs`)。子の `mod` を持ち得るなら、取り込み先の
    // ディレクトリごと保留する。マクロの定義は木の全体を読んでから決める。
    if assembled.path && assembled.module {
        bare_hashes.append(&mut assembled.hashes);
    }
    let mut capable = module_macros(&macro_definitions, &macro_aliases);
    // `quote!` の生成するコードの `#x` は属性にならない。ただし同じ名前を木の中のマクロに付けて
    // いれば (別名を含む)、そのマクロへ渡る。
    let quote_like = |name: &Option<String>| {
        name.as_deref()
            .is_some_and(|name| QUOTE_MACROS.contains(&name))
    };
    if !capable.is_empty()
        || !include_fields.is_empty()
        || !array_macros.is_empty()
        || bare_hashes
            .iter()
            .any(|(in_definition, name)| !in_definition && quote_like(name))
    {
        // 別名だけを置いたファイル (`use crate::children as mount;`・マクロの引数の
        // `pass! { pub(crate) use crate::children as mount; }`) は前置きの絞り込みが読まないので、
        // 木の中のどの `.rs` の別名と、マクロの token tree の `use` も集める。
        // 信じる木の外のマクロの名前が、どこかの token tree に現れるかも集める。
        aliases_in_macros |=
            collect_use_aliases(&root, &mut macro_aliases, &mut tree_words, &array_macros)?;
        capable = module_macros(&macro_definitions, &macro_aliases);
    }
    if generates_macros || (aliases_in_macros && !capable.is_empty()) {
        capable.insert(ANY_MACRO.to_owned());
    }
    // 属性の角括弧の前でない `#` は、マクロの定義の中か、`quote!` 以外 (木の中のマクロ・名前を
    // 決められない `$m!`・木の外のマクロ) へ渡るなら、別のマクロで角括弧と `mod` に組み合わさって
    // `#[path]` になり得る (`mount!(#, path)` が `emit!($h, [$p = "..."])` を経て `$h $br mod copied;`)。
    let in_tree = |name: &str| {
        macro_definitions.contains_key(name) || macro_aliases.iter().any(|(alias, _)| alias == name)
    };
    // 信じる木の外のマクロの名前は、木の中のマクロに付いておらず (定義・別名)、どの token tree にも
    // 現れないこと。マクロが出力できる語は本体と引数の語だけなので、名前が token tree に無ければ、
    // マクロはその名前のマクロを作れず (`pass! { macro_rules! quote { .. } }`)、その名前を別名にも
    // できない (`pass! { use body as quote; }`)。
    let trusted = |name: &str| !in_tree(name) && !tree_words.contains(name);
    // [path] は標準マクロの式引数に限る。別マクロの引数に同じ名前が出ていても、
    // 木にマクロ定義や use を生成する字句がなければその名前を新しく束縛できない。
    if array_macros.iter().any(|name| {
        !STANDARD_EXPRESSION_MACROS.contains(&name.as_str())
            || in_tree(name)
            || tree_words.contains(name) && (generates_macros || aliases_in_macros)
    }) {
        return None;
    }
    if bare_hashes.iter().any(|(in_definition, name)| {
        *in_definition || !quote_like(name) || !name.as_deref().is_some_and(trusted)
    }) {
        return None;
    }
    // フィールドに見える `loader.include` も、木の中のマクロに渡れば `include!` になり得る
    // (`$op!($p)`・`!` を別の引数で渡す `load!(loader.include, !, (..))`)。引数を式として読む
    // 標準のマクロ (`println!("{}", opts.include)`) に渡るものだけを許す。
    if include_fields.iter().any(|(in_definition, name)| {
        *in_definition
            || !name
                .as_deref()
                .is_some_and(|name| STANDARD_EXPRESSION_MACROS.contains(&name) && trusted(name))
    }) {
        return None;
    }
    for target in path_targets {
        let children = target.file_name().is_some_and(|name| name == "mod.rs")
            || may_declare_child_modules(&target, &capable);
        relocated.push(if children {
            target.parent()?.to_owned()
        } else {
            target.with_extension("")
        });
    }
    relocated.sort();
    relocated.dedup();
    for (file, source, offsets) in candidates {
        let tree = parser::parse_source(&source, LangId::Rust).ok()?;
        let relative = file.strip_prefix(&root).ok()?.to_str()?;
        for offset in offsets {
            let node = tree
                .root_node()
                .descendant_for_byte_range(offset, offset + "include".len())?;
            if !super::format_owner::stock_expression(
                node, &source, &root, relative, project, &relocated,
            ) {
                return None;
            }
        }
    }
    project
        .macros_may_export_tokens
        .get_or_init(|| generates_macros || open || external_macros);
    let macros = !macro_definitions.is_empty() || generates_macros || open;
    Some((relocated, included, macros))
}

/// 構文木の `shebang` の節が、rustc では shebang でないものか: ファイルの先頭 (UTF-8 の BOM の後)
/// でない (先頭のコメントの後の `#! [..]`。rustc の shebang はファイルの先頭だけ) か、
/// [`hidden_inner_attribute`] の形。
fn hidden_shebang(root: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    let start = if source.starts_with(b"\xEF\xBB\xBF") {
        3
    } else {
        0
    };
    crate::engine::tree_walk::any_node(root, crate::engine::tree_walk::Children::All, |node| {
        node.kind() == "shebang"
            && (node.start_byte() != start
                || source
                    .get(node.start_byte()..node.end_byte())
                    .is_some_and(hidden_inner_attribute))
    })
}

/// ファイルの先頭の `#!` の行 (UTF-8 の BOM の後も) が、rustc では内側の属性になり得るか: `#!` の
/// 直後でない位置に `[` があるか、コメントの始まり (`/*`・`//`) がある。rustc は `#!` の後に空白か
/// コメント (行をまたぐ `/* .. */` も) を挟んで `[` が続けば内側の属性として読むが、同梱の文法は
/// `#!` の直後が `[` でない行の全体を shebang にする。直後が `[` の普通の内側の属性
/// (`#![cfg(..)]`) と、`[` もコメントも無い普通の shebang (`#!/usr/bin/env rust-script`) は
/// 当たらない。
fn hidden_inner_attribute(source: &[u8]) -> bool {
    source
        .strip_prefix(b"\xEF\xBB\xBF".as_slice())
        .unwrap_or(source)
        .strip_prefix(b"#!".as_slice())
        .is_some_and(|rest| {
            if rest.first() == Some(&b'[') {
                return false;
            }
            let line = rest.split(|&byte| byte == b'\n').next().unwrap_or(rest);
            line.contains(&b'[')
                || memchr::memmem::find(line, b"/*").is_some()
                || memchr::memmem::find(line, b"//").is_some()
        })
}

/// `#[path]` で読んだファイルが、別のソースを取り込み得るか (`include!`・`#[path]`・それを作り得る
/// マクロ)。読めない・構文を解けなければ真。
fn may_include_sources(target: &Path) -> bool {
    let Some(source) = read_input_file(target) else {
        return true;
    };
    if !nfc_source(&source) {
        return true;
    }
    let Ok(tree) = parser::parse_source(&source, LangId::Rust) else {
        return true;
    };
    tree.root_node().has_error()
        || crate::engine::rust_dynamic_source::tree_includes_external_source(
            tree.root_node(),
            &source,
        )
}

/// 木の中 (root の直下の `.git`・`target` を除く) のどの `.rs` の `use ... as ...` も、(別名, 元の名前の
/// 最後の要素) として `aliases` に集め、マクロの token tree (引数・本体) に現れる信じる木の外の
/// マクロの名前 ([`TRUSTED_MACROS`]) を `words` に集める。マクロの token tree に `use` があるか
/// ([`token_tree_has_use`]) を返す。読めない・NFC でないソースがあれば `None`。
fn collect_use_aliases(
    root: &Path,
    aliases: &mut Vec<(String, String)>,
    words: &mut HashSet<String>,
    array_macros: &HashSet<String>,
) -> Option<bool> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory).ok()? {
            let entry = entry.ok()?;
            let kind = entry.file_type().ok()?;
            let path = entry.path();
            if kind.is_dir() {
                if directory != root
                    || !matches!(entry.file_name().to_str(), Some(".git" | "target"))
                {
                    pending.push(path);
                }
                continue;
            }
            if kind.is_file()
                && path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("rs"))
            {
                files.push(path);
            }
        }
    }
    let mut uses_in_macros = false;
    for path in files {
        // 字面で絞らずに読む (`as` の前後は改行やコメントでもよい)。
        let source = read_input_file(&path)?;
        if memchr::memmem::find(&source, b"as").is_none()
            && memchr::memmem::find(&source, b"use").is_none()
            && !TRUSTED_MACROS
                .iter()
                .any(|name| memchr::memmem::find(&source, name.as_bytes()).is_some())
        {
            continue;
        }
        if !nfc_source(&source) {
            return None;
        }
        let tree = parser::parse_source(&source, LangId::Rust).ok()?;
        let result = crate::engine::tree_walk::walk_preorder(
            tree.root_node(),
            crate::engine::tree_walk::Children::Named,
            |node, _| {
                if !array_macros.is_empty()
                    && node.kind() == "use_declaration"
                    && use_names(node, &source, true)
                        .is_none_or(|names| names.iter().any(|n| array_macros.contains(n)))
                {
                    return std::ops::ControlFlow::Break(());
                }
                if let Some(alias) = use_alias(node, &source) {
                    aliases.push(alias);
                }
                uses_in_macros |= token_list(node) && token_tree_has_use(node);
                if token_list(node) {
                    collect_trusted_words(node, &source, words);
                }
                std::ops::ControlFlow::Continue(crate::engine::tree_walk::Walk::Descend)
            },
        );
        if result.is_break() {
            return None;
        }
    }
    Some(uses_in_macros)
}

/// `use ... as ...` の (別名, 元の名前の最後の要素)。元の名前は構文木の最後の要素から読む
/// (`crate:: /* 注 */ children` のように途中にコメントがあっても `children`)。
fn use_alias(node: Node<'_>, source: &[u8]) -> Option<(String, String)> {
    if node.kind() != "use_as_clause" {
        return None;
    }
    let path = node.child_by_field_name("path")?;
    let alias = node.child_by_field_name("alias")?.utf8_text(source).ok()?;
    let last = match path.kind() {
        "scoped_identifier" => path.child_by_field_name("name")?,
        _ => path,
    };
    let original = last.utf8_text(source).ok()?;
    Some((
        alias.strip_prefix("r#").unwrap_or(alias).to_owned(),
        original.strip_prefix("r#").unwrap_or(original).to_owned(),
    ))
}

/// 字面の前置きの絞り込み: 空白を挟んで `[` (か `!` と `[`) が続かない `#` の字があるか (属性だけの
/// ファイルは読まない。生の文字列・生の識別子・コメントの `#` は読んでから構文で除く)。
fn may_have_bare_hash(source: &[u8]) -> bool {
    let next = |from: usize| {
        source
            .get(from..)
            .and_then(|rest| rest.iter().position(|byte| !byte.is_ascii_whitespace()))
            .map(|offset| from + offset)
    };
    memchr::memchr_iter(b'#', source).any(|index| match next(index + 1) {
        Some(at) if source[at] == b'[' => false,
        Some(at) if source[at] == b'!' => next(at + 1).is_none_or(|after| source[after] != b'['),
        _ => true,
    })
}

/// マクロの本体・引数の中にある属性の `#` と、別の角括弧から `#[path] mod` を組み直すのに
/// 必要な語。matcher が `tt` を別々に受け取れば `#[doc]` の `#` だけも転送できる。
#[derive(Default)]
struct AttributeInputs {
    hashes: Vec<(bool, Option<String>)>,
    path: bool,
    module: bool,
}

struct AttributeMacroScope {
    depth: usize,
    in_definition: bool,
    destination: Option<String>,
    recorded_hash: bool,
    simple: bool,
}

impl AttributeInputs {
    fn observe(
        &mut self,
        node: Node<'_>,
        depth: usize,
        macro_scopes: &mut Vec<AttributeMacroScope>,
        source: &[u8],
    ) {
        while macro_scopes
            .last()
            .is_some_and(|scope| depth <= scope.depth)
        {
            macro_scopes.pop();
        }
        if matches!(node.kind(), "macro_invocation" | "macro_definition") {
            let enclosing = macro_scopes.last();
            let destination = if node.kind() == "macro_invocation" {
                last_macro_identifier(node, source).map(str::to_owned)
            } else {
                enclosing.and_then(|scope| scope.destination.clone())
            };
            macro_scopes.push(AttributeMacroScope {
                depth,
                in_definition: node.kind() == "macro_definition"
                    || enclosing.is_some_and(|scope| scope.in_definition),
                destination,
                recorded_hash: false,
                simple: node
                    .child_by_field_name("macro")
                    .is_some_and(|m| m.kind() == "identifier"),
            });
        }
        let Some(scope) = macro_scopes.last_mut() else {
            return;
        };
        if !token_list(node) {
            return;
        }
        let mut cursor = node.walk();
        let mut hash = false;
        for child in node.children(&mut cursor) {
            hash |= child.kind() == "#"
                || (matches!(child.kind(), "line_comment" | "block_comment") && {
                    let mut cursor = child.walk();
                    child
                        .named_children(&mut cursor)
                        .any(|part| part.kind() == "doc_comment")
                });
            for word in word_token(child, source)
                .into_iter()
                .chain(hidden_words(child, source))
            {
                self.path |= word == "path";
                self.module |= word == "mod";
            }
        }
        for word in hidden_words(node, source) {
            self.path |= word == "path";
            self.module |= word == "mod";
        }
        if hash && !scope.recorded_hash {
            self.hashes
                .push((scope.in_definition, scope.destination.clone()));
            scope.recorded_hash = true;
        }
    }
}

/// token tree の中に、属性の角括弧 (`#[`・`#![`) の前でない `#` があるか (`mount!(#, path)`)。
fn has_bare_hash(node: Node<'_>, source: &[u8]) -> bool {
    let mut cursor = node.walk();
    let tokens: Vec<_> = node
        .children(&mut cursor)
        .filter(|child| !matches!(child.kind(), "line_comment" | "block_comment"))
        .collect();
    let bracket = |token: Option<&Node<'_>>| {
        token.is_some_and(|token| {
            token.kind() == "token_tree"
                && token.child(0).and_then(|open| open.utf8_text(source).ok()) == Some("[")
        })
    };
    tokens.iter().enumerate().any(|(index, token)| {
        token.kind() == "#"
            && !bracket(tokens.get(index + 1))
            && !(tokens.get(index + 1).is_some_and(|next| next.kind() == "!")
                && bracket(tokens.get(index + 2)))
    })
}

/// token tree の中の字句 (裸の `#` を含む token tree・フィールドに見える `include`) の行き先:
/// (マクロの定義の中か, 渡す先のマクロの名前)。名前は、それを囲む最も内側の実際の呼び出し
/// (構文木の `macro_invocation`) の最後の名前。引数の中の `quote!(#)` や `name!(..)` は字句のまま
/// 外側のマクロに渡る (`wrap!(quote!(#), path)` は `wrap` が `#` を受け取る) ので、引数の中の
/// `name !` の並びは行き先にしない。
fn macro_destination(node: Node<'_>, source: &[u8]) -> (bool, Option<String>) {
    let mut ancestor = node.parent();
    let mut in_definition = false;
    let mut destination: Option<Option<String>> = None;
    while let Some(current) = ancestor {
        in_definition |= current.kind() == "macro_definition";
        if destination.is_none() && current.kind() == "macro_invocation" {
            destination = Some(last_macro_identifier(current, source).map(str::to_owned));
        }
        ancestor = current.parent();
    }
    (in_definition, destination.flatten())
}

/// フィールドに見える `include` の行き先 ([`macro_destination`])。外側の呼び出しの引数に入れ子の
/// マクロの呼び出し (`println!("{}", value!(loader.include, ..))` の `value!`) があれば、その中で
/// 展開され得るので、行き先を決めない (`None`)。
fn include_destination(node: Node<'_>, source: &[u8]) -> (bool, Option<String>) {
    let (in_definition, destination) = macro_destination(node, source);
    let mut invocation = node.parent();
    while let Some(current) = invocation {
        if current.kind() == "macro_invocation" {
            break;
        }
        invocation = current.parent();
    }
    let nested = invocation.is_none_or(|invocation| {
        crate::engine::tree_walk::any_node(
            invocation,
            crate::engine::tree_walk::Children::All,
            |token| {
                token.kind() == "!"
                    && token.parent().is_some_and(token_list)
                    && token
                        .prev_sibling()
                        .is_some_and(|name| word_token(name, source).is_some())
                    && token
                        .next_sibling()
                        .is_some_and(|args| args.kind() == "token_tree")
            },
        )
    });
    (in_definition, destination.filter(|_| !nested))
}

/// 木の文法が識別子の葉に出さない `include` の語: 束縛の無いメタ変数 `$include` (`$` と `include`
/// に分かれて次のマクロへ渡る) と、繰り返しの区切り (`$($x)include*`。同梱の文法は区切りを木に
/// 出さない)。
fn hidden_include(node: Node<'_>, source: &[u8]) -> bool {
    hidden_words(node, source).contains(&"include")
}

/// メタ変数の名前 (`$include` の `include`) と、繰り返しの区切りの語。
fn hidden_words<'s>(node: Node<'_>, source: &'s [u8]) -> Vec<&'s str> {
    let word = |text: &'s str| {
        let text = text.trim();
        let text = text.strip_prefix("r#").unwrap_or(text);
        let mut chars = text.chars();
        let ok = |c: char| c == '_' || c.is_alphanumeric() || !c.is_ascii();
        (chars
            .next()
            .is_some_and(|first| ok(first) && !first.is_ascii_digit())
            && chars.all(ok))
        .then_some(text)
    };
    match node.kind() {
        "metavariable" => node
            .utf8_text(source)
            .ok()
            .and_then(|text| text.strip_prefix('$'))
            .and_then(word)
            .into_iter()
            .collect(),
        "token_repetition" => {
            // `$ ( .. ) 区切り 演算子` の、閉じる `)` と演算子の間。
            let mut cursor = node.walk();
            let children: Vec<_> = node.children(&mut cursor).collect();
            let close = children.iter().rev().find(|child| child.kind() == ")");
            let operator = children.last();
            match (close, operator) {
                (Some(close), Some(operator)) if close.end_byte() <= operator.start_byte() => {
                    source
                        .get(close.end_byte()..operator.start_byte())
                        .and_then(|text| std::str::from_utf8(text).ok())
                        .and_then(word)
                        .into_iter()
                        .collect()
                }
                _ => Vec::new(),
            }
        }
        _ => Vec::new(),
    }
}

/// 生成するコードの中の `#x` が属性にならない `quote` の仲間。引数のどこも展開しないものだけ
/// (`quote_spanned!` の先頭の span は展開する式なので、中のマクロが `#` を受け取り得る)。
const QUOTE_MACROS: &[&str] = &["quote", "parse_quote"];

/// 引数を式 (と書式の引数・パターン) として読み、項目もマクロの呼び出しも作らない標準のマクロ。
const STANDARD_EXPRESSION_MACROS: &[&str] = &[
    "assert",
    "assert_eq",
    "assert_ne",
    "dbg",
    "debug_assert",
    "debug_assert_eq",
    "debug_assert_ne",
    "eprint",
    "eprintln",
    "format",
    "format_args",
    "matches",
    "panic",
    "print",
    "println",
    "todo",
    "unimplemented",
    "unreachable",
    "vec",
    "write",
    "writeln",
];

/// 信じる木の外のマクロの名前 ([`QUOTE_MACROS`] と [`STANDARD_EXPRESSION_MACROS`])。
const TRUSTED_MACROS: &[&str] = &[
    "quote",
    "parse_quote",
    "assert",
    "assert_eq",
    "assert_ne",
    "dbg",
    "debug_assert",
    "debug_assert_eq",
    "debug_assert_ne",
    "eprint",
    "eprintln",
    "format",
    "format_args",
    "matches",
    "panic",
    "print",
    "println",
    "todo",
    "unimplemented",
    "unreachable",
    "vec",
    "write",
    "writeln",
];

/// token tree (繰り返しを含む) の直下の語 (メタ変数の名前と繰り返しの区切りも) のうち、
/// [`TRUSTED_MACROS`] の名前を `words` に足す。
fn collect_trusted_words(node: Node<'_>, source: &[u8], words: &mut HashSet<String>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let found = word_token(child, source)
            .into_iter()
            .chain(hidden_words(child, source));
        for word in found {
            if TRUSTED_MACROS.contains(&word) {
                words.insert(word.to_owned());
            }
        }
    }
    for word in hidden_words(node, source) {
        if TRUSTED_MACROS.contains(&word) {
            words.insert(word.to_owned());
        }
    }
}

/// ソースの文字が NFC か (ASCII だけなら真)。Rust は識別子を NFC で比べるので、NFC でない綴り
/// (`cafe\u{301}` と `caf\u{e9}` は同じ名前) のあるファイルでは、名前を字面で照合できない。
fn nfc_source(source: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(source) else {
        return false;
    };
    if source.is_ascii() || unicode_normalization::is_nfc(text) {
        return true;
    }
    let Ok(tree) = parser::parse_source(source, LangId::Rust) else {
        return false;
    };
    let root = tree.root_node();
    if root.has_error() {
        return false;
    }
    // 識別子として解釈されない文字列と通常コメントの中身だけを読み飛ばす。
    let mut end = 0;
    let result = crate::engine::tree_walk::walk_preorder(
        root,
        crate::engine::tree_walk::Children::Named,
        |node, _| {
            if matches!(node.kind(), "string_literal" | "raw_string_literal")
                || (matches!(node.kind(), "line_comment" | "block_comment")
                    && !node
                        .children(&mut node.walk())
                        .any(|n| n.kind().contains("doc_comment")))
            {
                if !unicode_normalization::is_nfc(&text[end..node.start_byte()]) {
                    return std::ops::ControlFlow::Break(());
                }
                end = node.end_byte();
                return std::ops::ControlFlow::Continue(crate::engine::tree_walk::Walk::Skip);
            }
            std::ops::ControlFlow::Continue(crate::engine::tree_walk::Walk::Descend)
        },
    );
    result.is_continue() && unicode_normalization::is_nfc(&text[end..])
}

/// 木の中の `macro_rules!` の定義 (同じ名前の定義はまとめる)。
#[derive(Default)]
struct MacroDefinition {
    /// 本体に単語の `mod` がある。
    mentions_mod: bool,
    /// 本体で呼ぶマクロの名前 (`$m!()` のように名前を決められなければ `None`)。
    invoked: Vec<Option<String>>,
}

/// マクロの定義の本体で呼び得るマクロの名前: 本体のどの識別子も (`children $(! $args)?` のように、
/// `!` が繰り返しや引数から来ても呼び出しになる)。名前を引数から受ける `$m!`・繰り返しの中で組み立てる
/// `$($p::children)? !()` は `None`。
fn invoked_macros(definition: Node<'_>, source: &[u8]) -> Vec<Option<String>> {
    let mut invoked = Vec::new();
    let _: std::ops::ControlFlow<()> = crate::engine::tree_walk::walk_preorder(
        definition,
        crate::engine::tree_walk::Children::Named,
        |node, _| {
            // 繰り返し (`$(children!($n);)*`) の中も数える。
            if token_list(node) {
                let mut cursor = node.walk();
                let tokens: Vec<_> = node
                    .children(&mut cursor)
                    .filter(|token| !token.is_extra())
                    .collect();
                for (index, token) in tokens.iter().enumerate() {
                    if let Some(name) = word_token(*token, source) {
                        invoked.push(Some(name.to_owned()));
                        continue;
                    }
                    match token.kind() {
                        "!" if index > 0
                            && matches!(
                                tokens[index - 1].kind(),
                                "metavariable" | "token_repetition"
                            ) =>
                        {
                            invoked.push(None);
                        }
                        _ => {}
                    }
                }
            }
            std::ops::ControlFlow::Continue(crate::engine::tree_walk::Walk::Descend)
        },
    );
    invoked
}

/// 呼び出しが子の `mod` を作り得る `macro_rules!` の名前。本体に `mod` の綴りがあるか、そうした
/// マクロか名前を決められないマクロ (`$m!`) を呼ぶもの (木の中の委譲と `use ... as` の別名を
/// たどる)。木の外のマクロ
/// (`params!` など) は、今までどおり子の `mod` を作らないとみなす。
fn module_macros(
    definitions: &HashMap<String, MacroDefinition>,
    aliases: &[(String, String)],
) -> HashSet<String> {
    let mut module_macros: HashSet<String> = definitions
        .iter()
        .filter(|(_, definition)| definition.mentions_mod)
        .map(|(name, _)| name.clone())
        .collect();
    loop {
        let before = module_macros.len();
        for (name, definition) in definitions {
            if module_macros.contains(name) {
                continue;
            }
            if definition.invoked.iter().any(|invoked| match invoked {
                None => true,
                Some(invoked) => module_macros.contains(invoked),
            }) {
                module_macros.insert(name.clone());
            }
        }
        // `use children as mount;` の別名も同じマクロ。
        for (alias, original) in aliases {
            if module_macros.contains(original) {
                module_macros.insert(alias.clone());
            }
        }
        if module_macros.len() == before {
            return module_macros;
        }
    }
}

/// `module_macros` に入れると、どの名前のマクロの呼び出しも子の `mod` を作り得るとする印 (マクロが
/// 定義するマクロの名前は分からない)。識別子にならない綴り。
const ANY_MACRO: &str = "!";

/// token tree と、マクロの本体の繰り返し (`$( .. )*`。中の `macro_rules!` も展開ごとに定義される)。
fn token_list(node: Node<'_>) -> bool {
    matches!(node.kind(), "token_tree" | "token_repetition")
}

/// manifest の lib が proc-macro か (`proc-macro = true`・`crate-type` に `proc-macro`)。
fn manifest_is_proc_macro(manifest: &toml::Value) -> bool {
    let Some(lib) = manifest.get("lib") else {
        return false;
    };
    lib.get("proc-macro")
        .or_else(|| lib.get("proc_macro"))
        .is_some_and(|value| value.as_bool() != Some(false))
        || lib
            .get("crate-type")
            .or_else(|| lib.get("crate_type"))
            .is_some_and(|types| {
                types.as_array().is_none_or(|types| {
                    types
                        .iter()
                        .any(|kind| kind.as_str().is_none_or(|kind| kind == "proc-macro"))
                })
            })
}

/// token tree (繰り返しを含む) の直下に `use` があるか (マクロが `use .. as mount;` を作り、子の `mod` を
/// 作るマクロに別の名前を付け得る。`as` は内側の波括弧 (`use crate::{children as mount};`) や引数から
/// 来得るので見ない)。
fn token_tree_has_use(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| child.kind() == "use")
}

/// token tree (繰り返しを含む) の直下に識別子の `macro_rules` があるか。
fn token_tree_names_macro_rules(node: Node<'_>, source: &[u8]) -> bool {
    // nightly の `macro` 項目 (`cfg_if! { pub macro mount() { .. } }`) も、引数から定義になり得る。
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|child| {
        child.kind() != "string_literal"
            && matches!(child.utf8_text(source).ok(), Some("macro_rules" | "macro"))
    })
}

/// テキストに単語の `mod` があるか。
fn mentions_mod(text: &str) -> bool {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|word| word == "mod")
}

/// `#[path]` で読んだファイルが子の `mod` を宣言し得るか。外部の `mod x;` か、マクロの引数や定義の
/// 中に `mod` の綴りがある、`module_macros` (子の `mod` を作り得る `macro_rules!`。別のファイルで
/// 定義して委譲するものを含む) を呼ぶとき。読めなければ真。
fn may_declare_child_modules(target: &Path, module_macros: &HashSet<String>) -> bool {
    let Some(source) = read_input_file(target) else {
        return true;
    };
    if !nfc_source(&source) {
        return true;
    }
    let Ok(tree) = parser::parse_source(&source, LangId::Rust) else {
        return true;
    };
    let root = tree.root_node();
    let any_macro = module_macros.contains(ANY_MACRO);
    root.has_error()
        || crate::engine::tree_walk::any_node(
            root,
            crate::engine::tree_walk::Children::Named,
            |node| {
                (node.kind() == "mod_item" && node.child_by_field_name("body").is_none())
                    // マクロが定義したマクロ (名前が分からない) は、引数の中の呼び出しも含めて数える。
                    || (any_macro && node.kind() == "macro_invocation")
                    || (any_macro
                        && node.kind() == "token_tree"
                        && node.utf8_text(&source).is_ok_and(|text| text.contains('!')))
                    // 直下の `children!();` は式の文として読まれるので、位置を問わず名前で見る。
                    || (node.kind() == "macro_invocation"
                        && last_macro_identifier(node, &source)
                            .is_none_or(|name| module_macros.contains(name)))
                    || (token_list(node) && {
                        let mut cursor = node.walk();
                        node.children(&mut cursor).any(|token| {
                            word_token(token, &source) == Some("mod")
                                || hidden_words(token, &source).contains(&"mod")
                        })
                    })
                    // 引数の中の `children!()` は、外側のマクロ (`pass! { children!(); }`) が字句の
                    // まま展開して呼び出す。
                    || (token_list(node) && token_tree_names_any(node, &source, module_macros))
            },
        )
}

/// token tree (繰り返しを含む) の直下に、`names` のどれかの名前の語 ([`word_token`]) があるか。
fn token_tree_names_any(node: Node<'_>, source: &[u8], names: &HashSet<String>) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| word_token(child, source).is_some_and(|name| names.contains(name)))
}

/// token tree の中の、識別子の綴りの葉 (`r#` を外す)。文法がキーワードとして返す綴り (`default`・
/// `union` など。`macro_rules! default` のようにマクロの名前になれる) と、ASCII でない文字を含む
/// 綴りも含む。
fn word_token<'s>(node: Node<'_>, source: &'s [u8]) -> Option<&'s str> {
    if node.child_count() != 0 {
        return None;
    }
    let text = node.utf8_text(source).ok()?;
    let name = text.strip_prefix("r#").unwrap_or(text);
    // ASCII でない文字は識別子に入り得るものとして数える (`a·b` の U+00B7 は XID_Continue。
    // 多く数えるほど保留する側)。
    let word = |c: char| c == '_' || c.is_alphanumeric() || !c.is_ascii();
    let mut chars = name.chars();
    (chars
        .next()
        .is_some_and(|first| word(first) && !first.is_ascii_digit())
        && chars.all(word))
    .then_some(name)
}

fn literal_include(node: Node<'_>, source: &[u8], file: &Path, root_dir: &Path) -> Option<PathBuf> {
    if node.kind() != "macro_invocation" || last_macro_identifier(node, source)? != "include" {
        return None;
    }
    let mut cursor = node.walk();
    let arguments = node
        .named_children(&mut cursor)
        .find(|child| child.kind() == "token_tree")?;
    let mut cursor = arguments.walk();
    let mut tokens = arguments.named_children(&mut cursor);
    let literal = tokens.next()?;
    if literal.kind() != "string_literal" || tokens.next().is_some() {
        return None;
    }
    let raw = literal
        .utf8_text(source)
        .ok()?
        .strip_prefix('"')?
        .strip_suffix('"')?;
    // 生の CR は、rustc が字句の前に CRLF を LF にするので、書いたバイト列と読むパスが違い得る。
    if raw.contains(['\\', '\r']) || Path::new(raw).is_absolute() || !raw.ends_with(".rs") {
        return None;
    }
    let target = file.parent()?.join(raw).canonicalize().ok()?;
    // ネストした取り込みや外部 mod の基準ディレクトリは、元ファイルだけでは決まらない。
    if super::under_root_git_or_target(target.strip_prefix(root_dir).ok()?) {
        return None;
    }
    let data = read_input_file(&target)?;
    let tree = parser::parse_source(&data, LangId::Rust).ok()?;
    let root = tree.root_node();
    if root.has_error()
        || crate::engine::tree_walk::any_node(
            root,
            crate::engine::tree_walk::Children::Named,
            |n| {
                n.kind() == "mod_item" && n.child_by_field_name("body").is_none()
                    || crate::engine::rust_dynamic_source::node_includes_external_source(n, &data)
            },
        )
    {
        return None;
    }
    Some(target)
}

fn path_target(attribute: Node<'_>, source: &[u8], file: &Path) -> Option<PathBuf> {
    if attribute_name(attribute, source) != Some("path") {
        return None;
    }
    let item = attribute.parent()?;
    if item.kind() != "attribute_item" || item.parent()?.kind() != "source_file" {
        return None;
    }
    let declaration = item.next_named_sibling()?;
    if declaration.kind() != "mod_item" || declaration.child_by_field_name("body").is_some() {
        return None;
    }
    let raw = attribute
        .child_by_field_name("value")?
        .utf8_text(source)
        .ok()?
        .strip_prefix('"')?
        .strip_suffix('"')?;
    // 生の CR は、rustc が字句の前に CRLF を LF にするので、書いたバイト列と読むパスが違い得る。
    if raw.is_empty() || raw.contains(['\\', '\r']) || Path::new(raw).is_absolute() {
        return None;
    }
    let lexical = file.parent()?.join(raw);
    // rustc は書いたパスの親から子の `mod` を探す。symlink なら実体の親と違うので扱わない。
    if std::fs::symlink_metadata(&lexical).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return None;
    }
    lexical.canonicalize().ok()
}

pub(super) struct EnumFile {
    variants: HashMap<String, HashSet<String>>,
}

fn name<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    let name = node.utf8_text(source).ok()?;
    Some(name.strip_prefix("r#").unwrap_or(name))
}

pub(super) fn attribute_name<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    let attribute = if node.kind() == "attribute" {
        node
    } else {
        node.named_child(0)?
    };
    name(attribute.named_child(0)?, source)
}

fn stable_item(item: Node<'_>, source: &[u8]) -> bool {
    stable_attributes(item, source, false)
}

fn stable_attributes(item: Node<'_>, source: &[u8], shadowable: bool) -> bool {
    let mut previous = item.prev_named_sibling();
    while let Some(node) = previous {
        match node.kind() {
            "line_comment" | "block_comment" => {}
            "attribute_item" => {
                let attribute = attribute_name(node, source);
                if matches!(attribute, Some("cfg" | "cfg_attr"))
                    || shadowable && matches!(attribute, Some("derive" | "test"))
                    || (attribute != Some("derive")
                        && super::super::rust_std_origin::safe_attribute(node, source).is_none())
                {
                    return false;
                }
            }
            _ => break,
        }
        previous = node.prev_named_sibling();
    }
    true
}

fn root_is_plain(root: Node<'_>, source: &[u8]) -> bool {
    if root.has_error() {
        return false;
    }
    let mut cursor = root.walk();
    !root.named_children(&mut cursor).any(|node| {
        node.kind() == "inner_attribute_item"
            && (matches!(attribute_name(node, source), Some("cfg" | "cfg_attr"))
                || super::super::rust_std_origin::safe_attribute(node, source).is_none())
    })
}

impl EnumFile {
    pub(super) fn contains(&self, ty: &str, variant: &str) -> bool {
        self.variants
            .get(ty)
            .is_some_and(|variants| variants.contains(variant))
    }

    pub(super) fn read(source: &[u8]) -> Option<Self> {
        let tree = parser::parse_source(source, LangId::Rust).ok()?;
        let root = tree.root_node();
        if !root_is_plain(root, source) {
            return None;
        }
        let mut variants = HashMap::new();
        let mut ambiguous = HashSet::new();
        let mut cursor = root.walk();
        let shadowable = root.named_children(&mut cursor).any(|item| {
            item.kind() == "extern_crate_declaration"
                || item.kind() == "use_declaration"
                    && root_use_names(item, source).is_none_or(|names| {
                        names.iter().any(|name| {
                            matches!(
                                crate::language::normalize_identifier(LangId::Rust, name).as_ref(),
                                "derive" | "test"
                            )
                        })
                    })
        });
        for item in root.named_children(&mut cursor) {
            let Some(identifier) = item
                .child_by_field_name("name")
                .and_then(|node| name(node, source))
            else {
                continue;
            };
            if item.kind() != "enum_item" || !stable_attributes(item, source, shadowable) {
                ambiguous.insert(identifier.to_owned());
                continue;
            }
            let body = item.child_by_field_name("body")?;
            let mut names = HashSet::new();
            let mut cursor = body.walk();
            for variant in body.named_children(&mut cursor) {
                if variant.kind() != "enum_variant" {
                    continue;
                }
                // variant の属性は helper 属性だが、cfg で消える形は証明しない。
                let mut previous = variant.prev_named_sibling();
                while let Some(node) = previous {
                    if node.kind() == "attribute_item" {
                        if matches!(attribute_name(node, source), Some("cfg" | "cfg_attr")) {
                            return None;
                        }
                    } else if !matches!(node.kind(), "line_comment" | "block_comment") {
                        break;
                    }
                    previous = node.prev_named_sibling();
                }
                if !names.insert(name(variant.child_by_field_name("name")?, source)?.to_owned()) {
                    return None;
                }
            }
            if variants.insert(identifier.to_owned(), names).is_some() {
                ambiguous.insert(identifier.to_owned());
            }
        }
        for name in ambiguous {
            variants.remove(&name);
        }
        Some(Self { variants })
    }
}

fn ordinary_file(dir: &Path, path: &Path) -> Option<bool> {
    // snapshot が書き出さなかった要素のために実際の checkout と有無が違いうるなら、決めない
    // (Issue #376)。
    if crate::git_support::tree_view(dir).hides(path) {
        return None;
    }
    match std::fs::symlink_metadata(dir.join(path)) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => dir
            .join(path)
            .canonicalize()
            .ok()?
            .starts_with(dir.canonicalize().ok()?)
            .then_some(true),
        Ok(_) => None,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(false),
        Err(_) => None,
    }
}

pub(super) fn ordinary_module_file(dir: &Path, src: &Path, modules: &[String]) -> Option<PathBuf> {
    module_file(dir, src, modules, false)
}

fn module_file(
    dir: &Path,
    src: &Path,
    modules: &[String],
    test_attribute: bool,
) -> Option<PathBuf> {
    let mut found = None;
    for root in [src.join("lib.rs"), src.join("main.rs")] {
        if !ordinary_file(dir, &root)? {
            continue;
        }
        let mut file = root;
        let mut prefix = src.to_owned();
        let mut reachable = true;
        for module in modules {
            let source = read_input_file(&dir.join(&file))?;
            let tree = parser::parse_source(&source, LangId::Rust).ok()?;
            let root = tree.root_node();
            if !root_is_plain(root, &source) {
                return None;
            }
            let mut cursor = root.walk();
            let declarations = root
                .named_children(&mut cursor)
                .filter(|node| {
                    node.kind() == "mod_item"
                        && node
                            .child_by_field_name("name")
                            .and_then(|node| name(node, &source))
                            == Some(module.as_str())
                })
                .collect::<Vec<_>>();
            if declarations.is_empty() {
                reachable = false;
                break;
            }
            let [declaration] = declarations.as_slice() else {
                return None;
            };
            let enabled_test = test_attribute
                && declaration.prev_named_sibling().is_some_and(|attr| {
                    attr.kind() == "attribute_item"
                        && attr.utf8_text(&source).ok() == Some("#[cfg(test)]")
                        && !has_attribute_prefix(attr)
                });
            if (has_attribute_prefix(*declaration) && !enabled_test)
                || declaration.child_by_field_name("body").is_some()
            {
                return None;
            }
            let flat = prefix.join(format!("{module}.rs"));
            let nested = prefix.join(module).join("mod.rs");
            file = match (ordinary_file(dir, &flat)?, ordinary_file(dir, &nested)?) {
                (true, false) => flat,
                (false, true) => nested,
                _ => return None,
            };
            prefix.push(module);
        }
        if reachable {
            if found.as_ref().is_some_and(|previous| previous != &file) {
                return None;
            }
            found = Some(file);
        }
    }
    found
}

fn crate_anchored(mut node: Node<'_>) -> bool {
    loop {
        match node.kind() {
            "crate" => return true,
            "scoped_identifier" | "scoped_use_list" | "use_as_clause" => {
                let Some(path) = node.child_by_field_name("path") else {
                    return false;
                };
                node = path;
            }
            _ => return false,
        }
    }
}

impl RustPathOwnerIndex {
    pub(in crate::engine::impact) fn enum_variant_owner(
        &self,
        node: Node<'_>,
        source: &[u8],
        dir: &Path,
        ref_path: &str,
        project: &RustPathOwnerProjectProof,
    ) -> Option<PathBuf> {
        if !self.same_source(source) || has_uncertain_ancestor(node) {
            return None;
        }
        let parent = node.parent()?;
        if parent.kind() == "enum_variant"
            && parent.child_by_field_name("name").map(|node| node.id()) == Some(node.id())
        {
            let declaration = parent.parent()?.parent()?;
            if declaration.kind() != "enum_item" || !stable_item(declaration, source) {
                return None;
            }
            let enum_name = name(declaration.child_by_field_name("name")?, source)?;
            let variant = name(node, source)?;
            return EnumFile::read(source)?
                .variants
                .get(enum_name)?
                .contains(variant)
                .then(|| PathBuf::from(ref_path));
        }
        if parent.kind() != "scoped_identifier"
            || parent.child_by_field_name("name").map(|node| node.id()) != Some(node.id())
            || !self.unshadowed_qualified_paths.contains(&parent.id())
        {
            return None;
        }
        let segments = path_segments(parent, source)?;
        let [base, variant] = segments.as_slice() else {
            return None;
        };
        let key = (base.clone(), variant.clone());
        if let Some(owner) = self.enum_owners.borrow().get(&key) {
            return owner.clone();
        }
        let result = (|| {
            let mut root = node;
            while let Some(parent) = root.parent() {
                root = parent;
            }
            let base = base.strip_prefix("r#").unwrap_or(base);
            let mut local = None;
            let mut imported = None;
            let mut cursor = root.walk();
            for item in root.named_children(&mut cursor) {
                if item
                    .child_by_field_name("name")
                    .and_then(|node| name(node, source))
                    == Some(base)
                    && local.replace(item).is_some()
                {
                    return None;
                }
                if item.kind() != "use_declaration" {
                    continue;
                }
                let names = root_use_names(item, source)?;
                if !names.iter().any(|name| name == base) {
                    continue;
                }
                if has_attribute_prefix(item) || imported.is_some() {
                    return None;
                }
                if !crate_anchored(item.child_by_field_name("argument")?) {
                    return None;
                }
                let mut edges = Vec::new();
                expand_rust_use_tree_edges_ast(
                    item.child_by_field_name("argument")?,
                    source,
                    &[],
                    &[],
                    &[],
                    None,
                    &mut edges,
                )?;
                for edge in edges {
                    if let RustPubUseEdge::Named {
                        exported_name,
                        target_module,
                        target_item,
                        origin: UsePathOrigin::Anchored,
                        ..
                    } = edge
                        && exported_name == base
                    {
                        imported = Some((target_module, target_item));
                    }
                }
                imported.as_ref()?;
            }
            let (file, enum_name) = match (local, imported) {
                (Some(local), None)
                    if local.kind() == "enum_item" && stable_item(local, source) =>
                {
                    (PathBuf::from(ref_path), base.to_owned())
                }
                (None, Some((modules, item))) => {
                    if !project.allows_static_source_path(Path::new(ref_path)) {
                        return None;
                    }
                    let (_, src, current) = source_module_path(dir, Path::new(ref_path))?;
                    if ordinary_module_file(dir, &src, &current)? != Path::new(ref_path) {
                        return None;
                    }
                    (ordinary_module_file(dir, &src, &modules)?, item)
                }
                _ => return None,
            };
            if file != Path::new(ref_path) && !project.allows_static_source_path(&file) {
                return None;
            }
            if !self.enum_files.borrow().contains_key(&file) {
                let parsed =
                    read_input_file(&dir.join(&file)).and_then(|source| EnumFile::read(&source));
                self.enum_files.borrow_mut().insert(file.clone(), parsed);
            }
            self.enum_files
                .borrow()
                .get(&file)?
                .as_ref()?
                .variants
                .get(&enum_name)?
                .contains(variant)
                .then_some(file)
        })();
        self.enum_owners.borrow_mut().insert(key, result.clone());
        result
    }
}

/// 参照ファイルから親へ順に並べた module のファイルと、crate root のファイル。参照ファイルが
/// crate root のときは `modules` は参照ファイルだけで、`roots` からは除く。
pub(super) fn ancestor_chain(dir: &Path, ref_path: &str) -> Option<(Vec<PathBuf>, Vec<PathBuf>)> {
    let (_, src, modules) = source_module_path(dir, Path::new(ref_path))?;
    if module_file(dir, &src, &modules, true)? != Path::new(ref_path) {
        return None;
    }
    let mut chain = vec![PathBuf::from(ref_path)];
    for count in (1..modules.len()).rev() {
        chain.push(module_file(dir, &src, &modules[..count], true)?);
    }
    let mut roots = Vec::new();
    for root in [src.join("lib.rs"), src.join("main.rs")] {
        if ordinary_file(dir, &root)? && !chain.contains(&root) {
            roots.push(root);
        }
    }
    Some((chain, roots))
}

pub(super) fn ancestor_files(dir: &Path, ref_path: &str) -> Option<Vec<PathBuf>> {
    let (_, src, modules) = source_module_path(dir, Path::new(ref_path))?;
    if ordinary_module_file(dir, &src, &modules)? != Path::new(ref_path) {
        return None;
    }
    let mut files = vec![PathBuf::from(ref_path)];
    for count in 1..modules.len() {
        files.push(ordinary_module_file(dir, &src, &modules[..count])?);
    }
    for root in [src.join("lib.rs"), src.join("main.rs")] {
        if ordinary_file(dir, &root)? {
            files.push(root);
        }
    }
    files.sort();
    files.dedup();
    Some(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> tree_sitter::Tree {
        parser::parse_source(source.as_bytes(), LangId::Rust).unwrap()
    }

    fn first<'t>(root: tree_sitter::Node<'t>, kind: &str) -> tree_sitter::Node<'t> {
        let mut found = None;
        crate::engine::tree_walk::any_node(root, crate::engine::tree_walk::Children::All, |node| {
            let hit = node.kind() == kind;
            if hit {
                found = Some(node);
            }
            hit
        });
        found.unwrap()
    }

    /// `#[path]`・`include!` の字面のパスの生の CR は、rustc が字句の前に CRLF を LF にするので、
    /// 書いたバイト列と読むパスが違い得る。取り込み先を決めない (呼び出し側で配置を決めない)。
    #[test]
    fn raw_carriage_returns_leave_literal_paths_undecided() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let src = root.join("a/src");
        std::fs::create_dir_all(&src).unwrap();
        // 書いた名前 (CR・LF を含む) のファイルがあっても決めない (Unix は名前に CR・LF を持てる。rustc は
        // CRLF を LF にした `x\ny.rs` を読む)。
        #[cfg(unix)]
        for name in ["x\r\ny.rs", "x\ny.rs"] {
            std::fs::write(src.join(name), "pub fn f() {}\n").unwrap();
        }
        let file = src.join("extra.rs");
        let source = "include!(\"x\r\ny.rs\");\n";
        let tree = parse(source);
        let include = first(tree.root_node(), "macro_invocation");
        assert_eq!(
            literal_include(include, source.as_bytes(), &file, &root),
            None
        );
        let source = "#[path = \"x\r\ny.rs\"]\nmod copied;\n";
        let tree = parse(source);
        let attribute = first(tree.root_node(), "attribute");
        assert_eq!(path_target(attribute, source.as_bytes(), &file), None);
    }

    /// `.rs` を読み得る `include_str!` (doctest の本体にできる) と、文書を読むもの。
    #[test]
    fn text_includes_of_rust_sources_are_open() {
        for (source, open) in [
            (
                "#[doc = concat!(\"```rust\\n\", include_str!(\"../../b/src/calls.rs\"), \"\\n```\")]\npub struct Guide;\n",
                true,
            ),
            ("const SRC: &str = std::include_str!(\"lib.RS\");\n", true),
            (
                "const SRC: &str = include_str!(concat!(\"calls\", \".rs\"));\n",
                true,
            ),
            (
                "const SRC: &str = include_str!(env!(\"SRC_PATH\"));\n",
                true,
            ),
            ("const SRC: &str = include_str!(\"calls.r\\x73\");\n", true),
            (
                "const SRC: &str = include_str!(concat!(\"../../b/src/calls.r\", 's'));\n",
                true,
            ),
            (
                "const SRC: &str = include_str!(concat!(\"calls.md\", stringify!(.rs)));\n",
                true,
            ),
            (
                "use std::include_str as read_src;\nconst SRC: &str = read_src!(\"../../b/src/calls.rs\");\n",
                true,
            ),
            (
                "const LICENSE: &str = include_str!(\"../LICENSE\");\n",
                false,
            ),
            (
                "const SRC: &str = include_str!(env!(\"DOC_SRC\", \"guide.md\"));\n",
                true,
            ),
            (
                "cfg_if::cfg_if! {\n    if #[cfg(all())] {\n        use std::include_str as read_src;\n    }\n}\nconst SRC: &str = read_src!(\"../../b/src/calls.rs\");\n",
                true,
            ),
            ("use std::fmt::{/* include_str */ Debug};\n", false),
            // マクロの名前を束縛しない位置の識別子は別名にならない。
            ("fn flags() {\n    let include_str = false;\n}\n", false),
            ("use std::include as include_str;\n", true),
            ("use std::include_str as include_bytes;\n", true),
            // Windows は要素の末尾の `.`・空白を落とし、`.` の要素を畳む。`:` はストリームの名前。
            (
                "const S: &str = include_str!(\"../../b/src/calls.rs.\");\n",
                true,
            ),
            (
                "const S: &str = include_str!(\"../../b/src/calls.rs/.\");\n",
                true,
            ),
            (
                "const S: &str = include_str!(\"../../b/src/calls.rs \");\n",
                true,
            ),
            (
                "const S: &str = include_str!(\"../../b/src/calls.rs::$DATA\");\n",
                true,
            ),
            (
                "const S: &str = include_str!(\"../../b/src/calls.rs/..\");\n",
                true,
            ),
            (
                "const S: &str = include_str!(\"../../b/src/calls.rs/unused/../.\");\n",
                true,
            ),
            (
                "const S: &str = include_str!(\"../../b/src/calls.rs/unused/.../\");\n",
                true,
            ),
            (
                "const S: &str = include_str!(\"../../README.md\");\n",
                false,
            ),
            (
                "const S: &str = include_str!(r#\"../../b/src/calls.rs\"#);\n",
                true,
            ),
            ("const S: &str = include_str!(\"../guide.md.\");\n", false),
            // `concat`・`env` は別名の `use` で付け替えられるので、マクロを含む引数はどれも読み得る。
            (
                "use std::env as concat;\nconst S: &str = include_str!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/guide.md\"));\n",
                true,
            ),
            // `include_bytes!` の結果は doctest の本体にならない。
            (
                "const ICON: &[u8] = include_bytes!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/assets/AppIcon.icns\"));\n",
                false,
            ),
            ("const SRC: &[u8] = include_bytes!(\"calls.rs\");\n", false),
            (
                "#[doc = include_str!(concat!(\"../\", \"README.md\"))]\npub struct Guide;\n",
                true,
            ),
            (
                "#[doc = include_str!(\"../README.md\")]\npub struct Guide;\n",
                false,
            ),
            ("const NAME: &str = \"calls.rs\";\n", false),
        ] {
            let tree = parse(source);
            assert_eq!(
                crate::engine::rust_dynamic_source::any_rust_text_include(
                    tree.root_node(),
                    source.as_bytes(),
                    |_| true
                ),
                open,
                "{source}"
            );
        }
    }
}
