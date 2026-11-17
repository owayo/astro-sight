//! C++ の単純な内部メソッドについて、旧新の実利用と値引数の契約を確かめる。

use std::collections::HashSet;
use std::ops::ControlFlow;

use tree_sitter::Node;

use super::signature::SignatureCandidate;
use super::*;
use crate::engine::tree_walk::{Children, Walk, walk_preorder};
use crate::language::LangId;

const MAX_SOURCE_BYTES: usize = 1024 * 1024;
const MAX_PROOF_NODES: usize = 100_000;
const MAX_BINDINGS: usize = 64;
const MAX_SCOPE_FILES: usize = 256;
const MAX_SCOPE_BYTES: usize = 8 * MAX_SOURCE_BYTES;

pub(super) fn prove(
    inputs: &DetectionInputs<'_>,
    old_path: &str,
    new_path: &str,
    candidate: &SignatureCandidate<'_>,
) -> bool {
    if candidate.kind != "method"
        || old_path != new_path
        || LangId::from_path(camino::Utf8Path::new(old_path)).ok() != Some(LangId::Cpp)
        || !single_prepared_method(inputs, new_path, candidate.name)
    {
        return false;
    }
    let Some((owner, member)) = candidate.name.split_once('.') else {
        return false;
    };
    if !external_uses_are_disjoint(inputs, new_path, owner, member) {
        return false;
    }
    let Some(sources) = load_new_source(inputs.dir, new_path)
        .and_then(|new| load_old_source_with_new(inputs.base_blobs, old_path, new_path, new))
    else {
        return false;
    };
    // 証明の予算を超えても API 変更は残す。巨大な AST を旧新で追加生成しない。
    if sources.old.len() > MAX_SOURCE_BYTES || sources.new.len() > MAX_SOURCE_BYTES {
        return false;
    }
    let Some((old_tree, new_tree)) = sources.parse_pair(LangId::Cpp) else {
        return false;
    };
    let Some(old) = file_proof(
        old_tree.root_node(),
        &sources.old,
        owner,
        member,
        candidate.old_sig,
    ) else {
        return false;
    };
    let Some(new) = file_proof(
        new_tree.root_node(),
        &sources.new,
        owner,
        member,
        candidate.new_sig,
    ) else {
        return false;
    };
    old.result == new.result
        && old.fields == new.fields
        && old.instances == new.instances
        && old.calls == new.calls
}

fn single_prepared_method(inputs: &DetectionInputs<'_>, path: &str, name: &str) -> bool {
    let Some((_, prepared)) = inputs
        .diff_files
        .iter()
        .zip(inputs.prepared)
        .find(|(file, _)| file.old_path == path && file.new_path == path)
    else {
        return false;
    };
    let PreparedDiffFile::Modified {
        old_syms: Some(old),
        new_syms: Some(new),
        ..
    } = prepared
    else {
        return false;
    };
    // 証明対象は単一メソッドだけ。既存の抽出結果で対象外と分かれば、候補ごとの全体走査を始めない。
    [old, new].into_iter().all(|symbols| {
        let mut methods = symbols.iter().filter(|(_, kind, _)| kind == "method");
        methods.next().is_some_and(|(method, _, _)| method == name) && methods.next().is_none()
    })
}

fn external_uses_are_disjoint(
    inputs: &DetectionInputs<'_>,
    path: &str,
    owner: &str,
    member: &str,
) -> bool {
    let index = inputs.ref_index;
    if index.added_reference_scan.parse_error_files != 0 {
        return false;
    }
    for name in [owner, member] {
        let Some(references) = index.complete_refs_for(name) else {
            return false;
        };
        if references.iter().any(|reference| {
            reference.kind == Some(crate::models::reference::RefKind::Unknown)
                || (crate::git_support::normalize_workspace_separators(&reference.path) != path
                    && (name == owner
                        || LangId::from_path(camino::Utf8Path::new(&reference.path)).ok()
                            != Some(LangId::Python)))
        }) {
            return false;
        }
    }
    let Ok(batch) =
        crate::engine::refs::SourceReferenceBatch::new(&[owner.to_owned(), member.to_owned()])
    else {
        return false;
    };
    let Some(root) = repository_root(inputs.dir) else {
        return false;
    };
    // ignore や生成物設定で隠れた追跡ソースと、旧版で消えた経路も調べる。
    for old in [true, false] {
        let Some(paths) = scope_paths(inputs, old) else {
            return false;
        };
        let mut bytes = 0;
        for other in paths {
            if other == path {
                continue;
            }
            let other_path = camino::Utf8Path::new(&other);
            if matches!(LangId::from_path(other_path), Ok(LangId::C | LangId::Cpp))
                || other_path
                    .extension()
                    .is_some_and(crate::engine::refs::is_unanalyzable_source_ext)
            {
                // include とマクロ展開後の参照までは証明しない。
                return false;
            }
            let source = if old {
                inputs.base_blobs.read_limited(&other, MAX_SOURCE_BYTES)
            } else {
                let full = root.join(&other);
                let local = std::fs::symlink_metadata(&full)
                    .is_ok_and(|metadata| metadata.is_file())
                    && std::fs::canonicalize(&full).is_ok_and(|full| full.starts_with(&root));
                local
                    .then(|| {
                        crate::engine::bounded_read::read_file_limited(
                            &full,
                            MAX_SOURCE_BYTES as u64,
                        )
                    })
                    .flatten()
            };
            let Some(source) = source else {
                return false;
            };
            bytes += source.len();
            if bytes > MAX_SCOPE_BYTES {
                return false;
            }
            let Ok(scan) = batch.scan(other_path, &source) else {
                return false;
            };
            if !scan.references[0].is_empty()
                || (!scan.references[1].is_empty()
                    && LangId::from_path(other_path).ok() != Some(LangId::Python))
            {
                return false;
            }
        }
    }
    true
}

fn scope_paths(inputs: &DetectionInputs<'_>, old: bool) -> Option<Vec<String>> {
    let args = if old {
        crate::commands::git_input::validate_git_revision(inputs.base_blobs.rev(), "git revision")
            .ok()?;
        vec!["ls-tree", "-r", "-z", inputs.base_blobs.rev()]
    } else {
        vec![
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ]
    };
    let output = git_output(inputs.dir, &args)?;
    let mut paths = std::collections::BTreeSet::new();
    for raw in output
        .split(|byte| *byte == 0)
        .filter(|raw| !raw.is_empty())
    {
        let text = std::str::from_utf8(raw).ok()?;
        let path = if old {
            let (metadata, path) = text.split_once('\t')?;
            let mut fields = metadata.split(' ');
            let (mode, kind, _) = (fields.next()?, fields.next()?, fields.next()?);
            if !matches!(mode, "100644" | "100755") || kind != "blob" || fields.next().is_some() {
                return None;
            }
            path
        } else {
            text
        };
        if path.contains(['\\', '\n', '\r']) || !crate::engine::impact::is_safe_diff_path(path) {
            return None;
        }
        paths.insert(path.to_owned());
        if paths.len() > MAX_SCOPE_FILES {
            return None;
        }
    }
    Some(paths.into_iter().collect())
}

fn repository_root(dir: &str) -> Option<std::path::PathBuf> {
    let root = std::fs::canonicalize(dir).ok()?;
    let top = git_output(dir, &["rev-parse", "--show-toplevel"])?;
    let top = std::str::from_utf8(&top)
        .ok()?
        .trim_end_matches(['\r', '\n']);
    let superproject = git_output(dir, &["rev-parse", "--show-superproject-working-tree"])?;
    (std::fs::canonicalize(top).ok()? == root && superproject.iter().all(u8::is_ascii_whitespace))
        .then_some(root)
}

fn git_output(dir: &str, args: &[&str]) -> Option<Vec<u8>> {
    use std::process::Stdio;
    let mut child = crate::git_support::git_command_for_dir(dir)
        .args(args)
        .current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let output = crate::commands::common::read_bytes_limited_and_drain(
        child.stdout.take()?,
        MAX_SOURCE_BYTES,
        "C++ closure git metadata",
    );
    if !child.wait().ok()?.success() {
        return None;
    }
    output.ok()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Primitive {
    Int,
    Bool,
}

impl Primitive {
    fn parse(node: Node<'_>, source: &[u8]) -> Option<Self> {
        if node.kind() != "primitive_type" {
            return None;
        }
        match node.utf8_text(source).ok()? {
            "int" => Some(Self::Int),
            "bool" => Some(Self::Bool),
            _ => None,
        }
    }

    fn accepts(self, node: Node<'_>, source: &[u8]) -> bool {
        match self {
            Self::Bool => matches!(node.kind(), "true" | "false"),
            Self::Int => {
                node.kind() == "number_literal"
                    && node.utf8_text(source).is_ok_and(|text| {
                        !text.is_empty()
                            && text.bytes().all(|byte| byte.is_ascii_digit())
                            && (text == "0" || !text.starts_with('0'))
                            && text.parse::<u16>().is_ok_and(|value| value <= 32767)
                    })
            }
        }
    }
}

#[derive(Debug)]
struct FileProof {
    result: Primitive,
    fields: Vec<(String, Primitive)>,
    instances: Vec<String>,
    calls: Vec<String>,
}

fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| child.kind() != "comment")
        .collect()
}

fn text_is(node: Node<'_>, source: &[u8], text: &str) -> bool {
    node.utf8_text(source).ok() == Some(text)
}

fn file_proof(
    root: Node<'_>,
    source: &[u8],
    owner: &str,
    member: &str,
    signature: &str,
) -> Option<FileProof> {
    if source.len() > MAX_SOURCE_BYTES
        || root.has_error()
        || !tree_within_budget(root, MAX_PROOF_NODES)
    {
        return None;
    }
    let mut namespace = None;
    let mut main = None;
    let mut cstdio = false;
    for node in children(root) {
        match node.kind() {
            "preproc_include"
                if children(node).len() == 1
                    && text_is(node.child_by_field_name("path")?, source, "<cstdio>") =>
            {
                cstdio = true;
            }
            "namespace_definition" if namespace.is_none() => namespace = Some(node),
            "function_definition" if main.is_none() => main = Some(node),
            _ => return None,
        }
    }
    let namespace = namespace?;
    let namespace_body = namespace.child_by_field_name("body")?;
    if children(namespace).len() != 1
        || normalize_signature_whitespace(
            &source[namespace.start_byte()..namespace_body.start_byte()],
        ) != "namespace"
    {
        return None;
    }
    let declarations = children(namespace_body);
    let [class] = declarations.as_slice() else {
        return None;
    };
    let class_name = class.child_by_field_name("name")?;
    let class_body = class.child_by_field_name("body")?;
    if class.kind() != "struct_specifier"
        || children(*class).len() != 2
        || !text_is(class_name, source, owner)
        || normalize_signature_whitespace(&source[class.start_byte()..class_body.start_byte()])
            != format!("struct {owner}")
    {
        return None;
    }
    let mut fields: Vec<(String, Primitive)> = Vec::new();
    let mut method = None;
    for node in children(class_body) {
        match node.kind() {
            "function_definition" if method.is_none() => method = Some(node),
            "field_declaration" => {
                let ty = Primitive::parse(node.child_by_field_name("type")?, source)?;
                let name = node.child_by_field_name("declarator")?;
                let value = node.child_by_field_name("default_value");
                if name.kind() != "field_identifier"
                    || children(node).len() != 2 + usize::from(value.is_some())
                    || value.is_some_and(|value| !ty.accepts(value, source))
                    || fields.iter().any(|(field, _)| text_is(name, source, field))
                {
                    return None;
                }
                fields.push((name.utf8_text(source).ok()?.to_owned(), ty));
                if fields.len() > MAX_BINDINGS {
                    return None;
                }
            }
            _ => return None,
        }
    }
    let method = method?;
    let method_body = method.child_by_field_name("body")?;
    let (method_name, result, parameters) = function_shape(method, source)?;
    if !text_is(method_name, source, member)
        || normalize_signature_whitespace(&source[method.start_byte()..method_body.start_byte()])
            != signature
        || fields
            .iter()
            .any(|(name, _)| name == member || name == owner)
        || parameters.iter().any(|(name, _)| {
            name == owner || name == member || fields.iter().any(|(field, _)| field == name)
        })
    {
        return None;
    }
    if !method_body_is_local(method_body, source, &fields, &parameters) {
        return None;
    }
    let main = main?;
    let main_body = main.child_by_field_name("body")?;
    if normalize_signature_whitespace(&source[main.start_byte()..main_body.start_byte()])
        != "int main()"
    {
        return None;
    }
    let mut allowed = HashSet::from([class_name.start_byte(), method_name.start_byte()]);
    let mut instances = Vec::new();
    let mut declarations = HashSet::new();
    for node in children(main_body) {
        if node.kind() != "declaration" {
            continue;
        }
        let Some(ty) = node.child_by_field_name("type") else {
            continue;
        };
        if ty.kind() != "type_identifier" || !text_is(ty, source, owner) {
            continue;
        }
        let name = node.child_by_field_name("declarator")?;
        let name_text = name.utf8_text(source).ok()?;
        if children(node).len() != 2
            || name.kind() != "identifier"
            || name_text == owner
            || name_text == member
            || instances.iter().any(|(name, _)| name == name_text)
        {
            return None;
        }
        allowed.extend([ty.start_byte(), name.start_byte()]);
        declarations.insert(node.start_byte());
        instances.push((name_text.to_owned(), node.end_byte()));
        if instances.len() > MAX_BINDINGS {
            return None;
        }
    }
    let mut calls = Vec::new();
    let valid = walk_preorder(main_body, Children::Named, |node, _| {
        if !MAIN_BODY_KINDS.contains(&node.kind())
            || (node.kind() == "declaration" && !declarations.contains(&node.start_byte()))
        {
            return ControlFlow::Break(());
        }
        if node.kind() == "call_expression"
            && let Some(function) = node.child_by_field_name("function")
        {
            if function.kind() == "identifier" && text_is(function, source, "printf") && cstdio {
                if let Some(receiver) = printf_value_receiver(node, source, &instances, &fields) {
                    allowed.extend([function.start_byte(), receiver.start_byte()]);
                    return ControlFlow::Continue(Walk::Descend);
                }
                return ControlFlow::Break(());
            }
            let Some((receiver, field)) = direct_field(function, source) else {
                return ControlFlow::Break(());
            };
            if !text_is(field, source, member) {
                return ControlFlow::Break(());
            }
            let Some(name) = instance_name(receiver, source, &instances) else {
                return ControlFlow::Break(());
            };
            let Some(arguments) = node.child_by_field_name("arguments") else {
                return ControlFlow::Break(());
            };
            let arguments = children(arguments);
            if arguments.len() != parameters.len()
                || !arguments
                    .iter()
                    .zip(&parameters)
                    .all(|(argument, (_, ty))| ty.accepts(*argument, source))
            {
                return ControlFlow::Break(());
            }
            allowed.extend([receiver.start_byte(), field.start_byte()]);
            calls.push(name.to_owned());
        }
        ControlFlow::Continue(Walk::Descend)
    });
    if valid.is_break() || calls.is_empty() {
        return None;
    }
    // すべての型名・メソッド名・実体名を照合し、登録・別名・持ち出し・未知の受け手を残す。
    let valid = walk_preorder(root, Children::Named, |node, _| {
        if matches!(
            node.kind(),
            "identifier" | "type_identifier" | "field_identifier"
        ) && node.utf8_text(source).is_ok_and(|text| {
            text == owner
                || text == member
                || text == "printf"
                || instances.iter().any(|(name, _)| name == text)
        }) && !allowed.contains(&node.start_byte())
        {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(Walk::Descend)
        }
    });
    valid.is_continue().then_some(FileProof {
        result,
        fields,
        instances: instances.into_iter().map(|(name, _)| name).collect(),
        calls,
    })
}

fn tree_within_budget(root: Node<'_>, limit: usize) -> bool {
    let mut nodes = 0;
    walk_preorder(root, Children::All, |_, _| {
        nodes += 1;
        if nodes > limit {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(Walk::Descend)
        }
    })
    .is_continue()
}

type Parameters = Vec<(String, Primitive)>;

fn function_shape<'a>(node: Node<'a>, source: &[u8]) -> Option<(Node<'a>, Primitive, Parameters)> {
    if children(node).len() != 3 {
        return None;
    }
    let result = Primitive::parse(node.child_by_field_name("type")?, source)?;
    let declarator = node.child_by_field_name("declarator")?;
    if declarator.kind() != "function_declarator" || children(declarator).len() != 2 {
        return None;
    }
    let name = declarator.child_by_field_name("declarator")?;
    if name.kind() != "field_identifier" {
        return None;
    }
    let mut parameters = Vec::new();
    for parameter in children(declarator.child_by_field_name("parameters")?) {
        if parameter.kind() != "parameter_declaration" || children(parameter).len() != 2 {
            return None;
        }
        let ty = Primitive::parse(parameter.child_by_field_name("type")?, source)?;
        let name = parameter.child_by_field_name("declarator")?;
        let name_text = name.utf8_text(source).ok()?;
        if name.kind() != "identifier" || parameters.iter().any(|(name, _)| name == name_text) {
            return None;
        }
        parameters.push((name_text.to_owned(), ty));
        if parameters.len() > MAX_BINDINGS {
            return None;
        }
    }
    Some((name, result, parameters))
}

const LOCAL_BODY_KINDS: &[&str] = &[
    "compound_statement",
    "expression_statement",
    "return_statement",
    "if_statement",
    "condition_clause",
    "else_clause",
    "assignment_expression",
    "binary_expression",
    "unary_expression",
    "parenthesized_expression",
    "conditional_expression",
    "identifier",
    "number_literal",
    "true",
    "false",
    "comment",
];

const MAIN_BODY_KINDS: &[&str] = &[
    "compound_statement",
    "declaration",
    "type_identifier",
    "identifier",
    "number_literal",
    "true",
    "false",
    "call_expression",
    "field_expression",
    "field_identifier",
    "argument_list",
    "expression_statement",
    "if_statement",
    "condition_clause",
    "else_clause",
    "return_statement",
    "unary_expression",
    "binary_expression",
    "parenthesized_expression",
    "conditional_expression",
    "string_literal",
    "string_content",
    "escape_sequence",
    "comment",
];

fn printf_value_receiver<'a>(
    call: Node<'a>,
    source: &[u8],
    instances: &[(String, usize)],
    fields: &[(String, Primitive)],
) -> Option<Node<'a>> {
    let arguments = children(call.child_by_field_name("arguments")?);
    let [format, value] = arguments.as_slice() else {
        return None;
    };
    let (receiver, field) = direct_field(*value, source)?;
    // <cstdio> の可変長引数は値渡し。未知の関数の参照引数へフィールドを渡す証拠にはしない。
    (text_is(*format, source, "\"%d\\n\"")
        && instance_name(receiver, source, instances).is_some()
        && fields
            .iter()
            .any(|(name, ty)| *ty == Primitive::Int && text_is(field, source, name)))
    .then_some(receiver)
}

fn method_body_is_local(
    body: Node<'_>,
    source: &[u8],
    fields: &[(String, Primitive)],
    parameters: &Parameters,
) -> bool {
    walk_preorder(body, Children::Named, |node, _| {
        if !LOCAL_BODY_KINDS.contains(&node.kind())
            || (node.kind() == "identifier"
                && !fields
                    .iter()
                    .chain(parameters)
                    .any(|(name, _)| text_is(node, source, name)))
        {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(Walk::Descend)
        }
    })
    .is_continue()
}

fn direct_field<'a>(node: Node<'a>, source: &[u8]) -> Option<(Node<'a>, Node<'a>)> {
    if node.kind() != "field_expression" || children(node).len() != 2 {
        return None;
    }
    let receiver = node.child_by_field_name("argument")?;
    let field = node.child_by_field_name("field")?;
    (receiver.kind() == "identifier"
        && field.kind() == "field_identifier"
        && source
            .get(receiver.end_byte()..field.start_byte())?
            .trim_ascii()
            == b".")
        .then_some((receiver, field))
}

fn instance_name<'a>(
    node: Node<'_>,
    source: &[u8],
    instances: &'a [(String, usize)],
) -> Option<&'a str> {
    instances.iter().find_map(|(name, declaration_end)| {
        (node.start_byte() >= *declaration_end && text_is(node, source, name))
            .then_some(name.as_str())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = "#include <cstdio>\nnamespace {\nstruct Tank {\nint total = 0;\nbool feed(int amount, bool keep) { if (keep) total += amount; return total > 0; }\n};\n}\nint main() { Tank tank; if (!tank.feed(3, true)) return 1; printf(\"%d\\n\", tank.total); return 0; }\n";

    fn check(source: &str, parse_error: bool) -> bool {
        let tree = parser::parse_source(source.as_bytes(), LangId::Cpp).unwrap();
        assert_eq!(tree.root_node().has_error(), parse_error, "{source}");
        let mut signature = None;
        let _: ControlFlow<()> = walk_preorder(tree.root_node(), Children::Named, |node, _| {
            if node.kind() != "function_definition" {
                return ControlFlow::Continue(Walk::Descend);
            }
            if let Some(body) = node.child_by_field_name("body") {
                let header = normalize_signature_whitespace(
                    &source.as_bytes()[node.start_byte()..body.start_byte()],
                );
                if header.contains("feed(") {
                    signature = Some(header);
                    return ControlFlow::Break(());
                }
            }
            ControlFlow::Continue(Walk::Descend)
        });
        file_proof(
            tree.root_node(),
            source.as_bytes(),
            "Tank",
            "feed",
            &signature.unwrap_or_else(|| panic!("method header missing: {source}")),
        )
        .is_some()
    }

    #[test]
    fn cpp_internal_proof_rejects_escape_and_unknown_contracts() {
        assert!(check(SOURCE, false));
        let cases = [
            ("public owner", SOURCE.replace("namespace {", "namespace public_api {")),
            ("inheritance", SOURCE.replace("struct Tank {", "struct Tank : Base {")),
            ("attribute", SOURCE.replace("struct Tank {", "struct [[nodiscard]] Tank {")),
            ("template", SOURCE.replace("struct Tank {", "template <class T> struct Tank {")),
            ("constructor", SOURCE.replace("int total = 0;", "Tank() {} int total = 0;")),
            ("destructor", SOURCE.replace("int total = 0;", "~Tank() {} int total = 0;")),
            ("overload", SOURCE.replace("int total = 0;", "bool feed() { return true; } int total = 0;")),
            ("pointer field", SOURCE.replace("int total = 0;", "int *total = 0;")),
            ("duplicate field", SOURCE.replace("int total = 0;", "int total = 0; bool total = false;")),
            ("pointer parameter", SOURCE.replace("int amount", "int *amount")),
            ("custom parameter", SOURCE.replace("int amount", "Count amount")),
            ("default parameter", SOURCE.replace("bool keep)", "bool keep = true)")),
            ("reference result", SOURCE.replace("bool feed", "bool& feed")),
            ("method call", SOURCE.replace("total += amount;", "store(total);")),
            ("field address", SOURCE.replace("return total > 0;", "return &total;")),
            ("this escape", SOURCE.replace("return total > 0;", "return this;")),
            ("body lambda", SOURCE.replace("total += amount;", "[&] { total += amount; }();")),
            ("type alias", SOURCE.replace("Tank tank;", "using Alias = Tank; Tank tank;")),
            ("type registration", SOURCE.replace("Tank tank;", "register_type<Tank>(); Tank tank;")),
            ("member pointer", SOURCE.replace("Tank tank;", "auto method = &Tank::feed; Tank tank;")),
            ("instance escape", SOURCE.replace("return 0;", "store(tank); return 0;")),
            ("instance address", SOURCE.replace("return 0;", "store(&tank); return 0;")),
            ("field reference", SOURCE.replace("printf(\"%d\\n\", tank.total)", "store(tank.total)")),
            ("main lambda", SOURCE.replace("if (!tank.feed(3, true)) return 1;", "[&] { return tank.feed(3, true); }();")),
            ("unknown receiver", SOURCE.replace("if (!tank.feed", "if (!other.feed")),
            ("same named other owner", SOURCE.replace("Tank tank;", "struct Bowl { bool feed(int, bool); }; Bowl bowl; bowl.feed(3, true); Tank tank;")),
            ("stale caller", SOURCE.replace("tank.feed(3, true)", "tank.feed(3)")),
            ("wrong literal type", SOURCE.replace("tank.feed(3, true)", "tank.feed(true, 3)")),
            ("nonliteral argument", SOURCE.replace("tank.feed(3, true)", "tank.feed(count, true)")),
            ("large literal", SOURCE.replace("tank.feed(3, true)", "tank.feed(999999999999, true)")),
            ("printf format", SOURCE.replace("%d\\n", "%n\\n")),
            ("printf local shadow", SOURCE.replace("Tank tank;", "int printf; Tank tank;")),
            ("printf owner shadow", SOURCE.replace("int total = 0;", "int printf = 0; int total = 0;")),
            ("printf alias", SOURCE.replace("Tank tank;", "auto print = printf; Tank tank;")),
            ("local include", SOURCE.replace("<cstdio>", "\"custom.h\"")),
            ("unknown include", SOURCE.replace("<cstdio>", "<other>")),
            ("missing include", SOURCE.replace("#include <cstdio>\n", "")),
            ("macro", format!("#define feed(...) invoke(__VA_ARGS__)\n{SOURCE}")),
            ("conditional", SOURCE.replace("struct Tank {", "#if FLAG\nstruct Tank {").replace("};\n}", "};\n#endif\n}")),
            ("main assembly", SOURCE.replace("return 0;", "asm(\"nop\"); return 0;")),
        ];
        for (name, source) in cases {
            assert!(!check(&source, false), "{name}: {source}");
        }
        assert!(!check(&SOURCE.replace("return 0;", "return (;"), true));
    }

    #[test]
    fn cpp_internal_proof_checks_the_candidate_signature() {
        let tree = parser::parse_source(SOURCE.as_bytes(), LangId::Cpp).unwrap();
        assert!(
            file_proof(
                tree.root_node(),
                SOURCE.as_bytes(),
                "Tank",
                "feed",
                "bool feed(int amount, bool keep)"
            )
            .is_some()
        );
        for (owner, member, signature) in [
            ("Other", "feed", "bool feed(int amount, bool keep)"),
            ("Tank", "other", "bool feed(int amount, bool keep)"),
            ("Tank", "feed", "bool feed(int amount)"),
        ] {
            assert!(
                file_proof(
                    tree.root_node(),
                    SOURCE.as_bytes(),
                    owner,
                    member,
                    signature
                )
                .is_none()
            );
        }
    }

    #[test]
    fn cpp_internal_proof_literals_require_decimal_int_and_exact_bool() {
        for argument in ["0", "3", "32767"] {
            assert!(
                check(
                    &SOURCE.replace(
                        "tank.feed(3, true)",
                        &format!("tank.feed({argument}, true)")
                    ),
                    false
                ),
                "{argument}"
            );
        }
        for argument in [
            "08",
            "09",
            "00",
            "01",
            "0x3",
            "3u",
            "32768",
            "999999999999",
            "3.0",
            "true",
            "-3",
        ] {
            assert!(
                !check(
                    &SOURCE.replace(
                        "tank.feed(3, true)",
                        &format!("tank.feed({argument}, true)")
                    ),
                    matches!(argument, "08" | "09")
                ),
                "{argument}"
            );
        }
    }

    #[test]
    fn cpp_internal_proof_budget_exhaustion_keeps_the_change_unproven() {
        let mut source = SOURCE.to_owned();
        source.extend(std::iter::repeat_n(' ', MAX_SOURCE_BYTES - source.len()));
        assert!(check(&source, false));
        source.push(' ');
        assert!(!check(&source, false));

        let tree = parser::parse_source(SOURCE.as_bytes(), LangId::Cpp).unwrap();
        let mut count = 0;
        let _: ControlFlow<()> = walk_preorder(tree.root_node(), Children::All, |_, _| {
            count += 1;
            ControlFlow::Continue(Walk::Descend)
        });
        assert!(tree_within_budget(tree.root_node(), count));
        assert!(!tree_within_budget(tree.root_node(), count - 1));
        let source = format!("{}{SOURCE}", "/* */".repeat(MAX_PROOF_NODES));
        assert!(!check(&source, false));

        for count in [MAX_BINDINGS - 1, MAX_BINDINGS] {
            let fields = (0..count)
                .map(|index| format!("int f{index} = 0;\n"))
                .collect::<String>();
            let source = SOURCE.replace("int total = 0;", &format!("{fields}int total = 0;"));
            assert_eq!(check(&source, false), count < MAX_BINDINGS);
        }
        for count in [MAX_BINDINGS, MAX_BINDINGS + 1] {
            let parameters = (2..count)
                .map(|index| format!(", bool p{index}"))
                .collect::<String>();
            let arguments = ", true".repeat(count - 2);
            let source = SOURCE
                .replace("bool keep)", &format!("bool keep{parameters})"))
                .replace(
                    "tank.feed(3, true)",
                    &format!("tank.feed(3, true{arguments})"),
                );
            assert_eq!(check(&source, false), count == MAX_BINDINGS);

            let instances = (1..count)
                .map(|index| format!("Tank tank{index};"))
                .collect::<String>();
            let source = SOURCE.replace("Tank tank;", &format!("Tank tank; {instances}"));
            assert_eq!(check(&source, false), count == MAX_BINDINGS);
        }
    }

    #[test]
    fn cpp_internal_proof_node_kinds_exist_in_cpp_grammar() {
        let grammar = LangId::Cpp.ts_language();
        for kind in LOCAL_BODY_KINDS.iter().chain(MAIN_BODY_KINDS).chain(
            [
                "preproc_include",
                "namespace_definition",
                "struct_specifier",
                "function_definition",
                "field_declaration",
                "primitive_type",
                "function_declarator",
                "parameter_declaration",
            ]
            .iter(),
        ) {
            assert!(
                tree_sitter::Query::new(&grammar, &format!("({kind}) @node")).is_ok(),
                "{kind}"
            );
        }
    }
}
