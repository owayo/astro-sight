//! 戻り型を変更した関数を、同じ差分で更新した callback 型へ渡す閉包証拠。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use tree_sitter::Node;

use super::signature_tokens::{SigTokens, node_signature_tokens, signature_tokens_in_range};
use super::source_pair::{CompatibleModSite, SignatureSourceCache};
use crate::commands::dead_code_default_liveness::{resolve_relative_specifier, static_specifier};
use crate::engine::{parser, refs};
use crate::language::LangId;
use crate::models::impact::DiffFile;

const MAX_FILES: usize = 512;
const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_NODES: usize = 100_000;
const MAX_DEPTH: usize = 128;

type Position = (String, usize, usize);

#[derive(Default)]
pub(super) struct HandoffCache {
    proof: Option<HandoffProof>,
}

impl HandoffCache {
    pub(super) fn covers(
        &mut self,
        inputs: &super::DetectionInputs<'_>,
        site: &CompatibleModSite<'_>,
        sources: &mut SignatureSourceCache<'_>,
    ) -> bool {
        if !matches!(site.lang_id, Some(LangId::Typescript | LangId::Tsx))
            || !matches!(site.kind, "function" | "interface" | "trait")
            || site.old_path != site.new_path
        {
            return false;
        }
        let proof = self.proof.get_or_insert_with(|| {
            HandoffProof::build(inputs.dir, inputs.base_blobs, inputs.diff_files)
                .unwrap_or_default()
        });
        let Some(pair) = sources.get(site) else {
            return false;
        };
        proof.covers(site.new_path, site.name)
            && proof
                .snapshots
                .get(site.new_path)
                .is_some_and(|(old, new)| {
                    *old == blake3::hash(&pair.old) && *new == blake3::hash(&pair.new)
                })
    }
}

/// この差分の全観測を確認した名前だけを保持する。一般的な互換性ではない。
#[derive(Default)]
pub(super) struct HandoffProof {
    closed: BTreeSet<(String, String)>,
    snapshots: BTreeMap<String, (blake3::Hash, blake3::Hash)>,
}

impl HandoffProof {
    pub(super) fn covers(&self, path: &str, name: &str) -> bool {
        self.closed.contains(&(path.to_owned(), name.to_owned()))
    }

    pub(super) fn build(
        dir: &str,
        blobs: &crate::commands::git_input::GitBlobBatch,
        diff: &[DiffFile],
    ) -> Option<Self> {
        let inventory = Inventory::load(dir, blobs, diff)?;
        let mut proof = Self::default();
        let mut attempts = 0usize;
        for file in diff {
            let pair = inventory.files.get(&file.new_path)?;
            for name in exported_names(&pair.new, "interface_declaration") {
                let Some((old_slot, new_slot)) = slot_pair(pair, &name) else {
                    continue;
                };
                let Some(consumer) = consumer_pair(pair, &name, &old_slot, &new_slot) else {
                    continue;
                };
                for producer_file in diff {
                    let producer_pair = inventory.files.get(&producer_file.new_path)?;
                    for producer_name in exported_names(&producer_pair.new, "function_declaration")
                    {
                        attempts += 1;
                        if attempts > 2048 {
                            return None;
                        }
                        let Some((old_producer, new_producer)) = producer_contract_pair(
                            producer_pair,
                            &producer_name,
                            &old_slot,
                            &new_slot,
                        ) else {
                            continue;
                        };
                        let scope = Scope {
                            producer_path: &producer_file.new_path,
                            producer: &producer_name,
                            interface_path: &file.new_path,
                            interface: &name,
                            consumer: &consumer.name,
                            property: &new_slot.name,
                        };
                        let old_calls = prove_side(
                            &inventory,
                            false,
                            &scope,
                            old_producer.name,
                            old_slot.declaration_name,
                            consumer.old_name,
                            consumer.old_type,
                        );
                        let new_calls = prove_side(
                            &inventory,
                            true,
                            &scope,
                            new_producer.name,
                            new_slot.declaration_name,
                            consumer.new_name,
                            consumer.new_type,
                        );
                        if old_calls.is_some() && old_calls == new_calls {
                            proof
                                .closed
                                .insert((producer_file.new_path.clone(), producer_name));
                            proof.closed.insert((file.new_path.clone(), name.clone()));
                        }
                    }
                }
            }
        }
        for (path, pair) in &inventory.files {
            proof.snapshots.insert(
                path.clone(),
                (
                    blake3::hash(&pair.old.source),
                    blake3::hash(&pair.new.source),
                ),
            );
        }
        Some(proof)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ContractType {
    Void,
    Primitive(String),
    Array { readonly: bool, element: Box<Self> },
}

impl ContractType {
    fn read(node: Node<'_>, source: &[u8], depth: usize) -> Option<Self> {
        if depth > 8 {
            return None;
        }
        match node.kind() {
            "type_annotation" | "parenthesized_type" => {
                Self::read(only_child(node)?, source, depth + 1)
            }
            "predefined_type" => match text(node, source)? {
                "void" => Some(Self::Void),
                name @ ("string" | "number" | "boolean" | "bigint") => {
                    Some(Self::Primitive(name.to_owned()))
                }
                _ => None,
            },
            "array_type" => {
                let element = Self::read(only_child(node)?, source, depth + 1)?;
                (element != Self::Void).then_some(Self::Array {
                    readonly: false,
                    element: Box::new(element),
                })
            }
            "readonly_type" => {
                let mut array = Self::read(only_child(node)?, source, depth + 1)?;
                let Self::Array { readonly, .. } = &mut array else {
                    return None;
                };
                *readonly = true;
                Some(array)
            }
            _ => None,
        }
    }

    fn accepts(&self, value: &Self) -> bool {
        match (self, value) {
            (
                Self::Array { readonly, element },
                Self::Array {
                    readonly: actual_readonly,
                    element: actual,
                },
            ) => (*readonly || !actual_readonly) && element.accepts(actual),
            _ => self == value,
        }
    }
}

struct SourceFile {
    source: Vec<u8>,
    tree: tree_sitter::Tree,
    occurrences: HashMap<String, Vec<(usize, usize)>>,
}

struct FilePair {
    old: SourceFile,
    new: SourceFile,
}

struct Inventory {
    files: BTreeMap<String, FilePair>,
    paths: HashMap<String, Vec<String>>,
}

impl Inventory {
    fn load(
        dir: &str,
        blobs: &crate::commands::git_input::GitBlobBatch,
        diff: &[DiffFile],
    ) -> Option<Self> {
        if diff.iter().any(|file| file.old_path != file.new_path) {
            return None;
        }
        let root = Path::new(dir).canonicalize().ok()?;
        let scan = refs::collect_files_scan(
            &root,
            None,
            refs::FileScanOptions {
                include_generated: true,
            },
        )
        .ok()?;
        if scan.files.len() > MAX_FILES || !scan.unanalyzable_truncations(&root).is_empty() {
            return None;
        }
        // refs の通常列挙が読み飛ばすリンクも、証明時は不明な入力として扱う。
        for entry in ignore::WalkBuilder::new(&root).hidden(true).build() {
            let entry = entry.ok()?;
            let metadata = std::fs::symlink_metadata(entry.path()).ok()?;
            if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
                return None;
            }
            if entry.file_type().is_some_and(|kind| kind.is_file()) {
                let name = entry.file_name().to_str()?;
                if entry.path().extension().is_none() {
                    parser::read_file(camino::Utf8Path::from_path(entry.path())?).ok()?;
                }
                if (name.starts_with("tsconfig") || name.starts_with("jsconfig"))
                    && name.ends_with(".json")
                {
                    let config = crate::engine::bounded_read::read_utf8_file_limited(
                        entry.path(),
                        1024 * 1024,
                    )?;
                    if ["extends", "rootDirs", "moduleSuffixes", "paths", "baseUrl"]
                        .iter()
                        .any(|word| config.contains(word))
                    {
                        return None;
                    }
                }
            }
        }
        let mut files = BTreeMap::new();
        let mut bytes = 0usize;
        let mut nodes = 0usize;
        for path in scan.files {
            let relative = crate::git_support::normalize_workspace_separators(
                &path.strip_prefix(&root).ok()?.to_string_lossy(),
            );
            let lang = LangId::from_path(camino::Utf8Path::from_path(&path)?).ok()?;
            if !matches!(lang, LangId::Typescript | LangId::Tsx)
                || relative.ends_with(".d.ts")
                || relative.ends_with(".d.mts")
                || relative.ends_with(".d.cts")
            {
                return None;
            }
            let source = parser::read_file(camino::Utf8Path::from_path(&path)?).ok()?;
            let old = blobs.read(&relative)?;
            bytes = bytes.checked_add(source.len())?.checked_add(old.len())?;
            if bytes > MAX_BYTES {
                return None;
            }
            let new = SourceFile::parse(source.to_vec(), lang, &mut nodes)?;
            let old = SourceFile::parse(old, lang, &mut nodes)?;
            files.insert(relative, FilePair { old, new });
        }
        let paths = files
            .keys()
            .map(|path| (path.clone(), Vec::new()))
            .collect();
        Some(Self { files, paths })
    }
}

#[cfg(windows)]
fn is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_: &std::fs::Metadata) -> bool {
    false
}

impl SourceFile {
    fn parse(source: Vec<u8>, lang: LangId, total_nodes: &mut usize) -> Option<Self> {
        let tree = parser::parse_source(&source, lang).ok()?;
        let mut occurrences: HashMap<String, Vec<(usize, usize)>> = HashMap::new();
        let mut pending = vec![(tree.root_node(), 0usize)];
        while let Some((node, depth)) = pending.pop() {
            *total_nodes = total_nodes.checked_add(1)?;
            if *total_nodes > MAX_NODES || depth > MAX_DEPTH || node.is_error() || node.is_missing()
            {
                return None;
            }
            if node.kind() == "comment" && text(node, &source)?.starts_with("/**") {
                return None;
            }
            if matches!(
                node.kind(),
                "identifier" | "type_identifier" | "property_identifier"
            ) && text(node, &source)?.contains('\\')
            {
                return None;
            }
            if node.is_named() && node.child_count() == 0 {
                let name = text(node, &source)?;
                if matches!(name, "eval" | "Function" | "require") {
                    return None;
                }
                occurrences
                    .entry(name.to_owned())
                    .or_default()
                    .push((node.start_byte(), node.end_byte()));
            }
            if matches!(
                node.kind(),
                "subscript_expression" | "import_alias" | "internal_module" | "ambient_declaration"
            ) {
                return None;
            }
            if node.kind() == "call_expression"
                && node
                    .child_by_field_name("function")
                    .is_some_and(|callee| callee.kind() == "import")
            {
                return None;
            }
            if node.kind() == "export_statement"
                && node.child_by_field_name("declaration").is_none()
            {
                return None;
            }
            if node.kind() == "import_statement" && !plain_import(node, &source) {
                return None;
            }
            let mut cursor = node.walk();
            pending.extend(node.children(&mut cursor).map(|child| (child, depth + 1)));
        }
        Some(Self {
            source,
            tree,
            occurrences,
        })
    }

    fn root(&self) -> Node<'_> {
        self.tree.root_node()
    }

    fn at(&self, start: usize, end: usize) -> Option<Node<'_>> {
        let node = self.root().descendant_for_byte_range(start, end)?;
        (node.start_byte() == start && node.end_byte() == end).then_some(node)
    }
}

fn text<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    node.utf8_text(source).ok()
}

fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| child.kind() != "comment")
        .collect()
}

fn only_child(node: Node<'_>) -> Option<Node<'_>> {
    let children = children(node);
    let [child] = children.as_slice() else {
        return None;
    };
    Some(*child)
}

fn plain_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    let name = text(node, source)?;
    (!name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$')))
    .then(|| name.to_owned())
}

fn plain_import(node: Node<'_>, source: &[u8]) -> bool {
    let Some(specifier) = node
        .child_by_field_name("source")
        .and_then(|node| static_specifier(node, source))
    else {
        return false;
    };
    if !(specifier.starts_with("./") || specifier.starts_with("../")) || specifier.contains('\\') {
        return false;
    }
    let Some(clause) = children(node)
        .into_iter()
        .find(|node| node.kind() == "import_clause")
    else {
        return false;
    };
    let Some(named) = only_child(clause).filter(|node| node.kind() == "named_imports") else {
        return false;
    };
    let mut names = BTreeSet::new();
    !children(named).is_empty()
        && children(named).into_iter().all(|specifier| {
            specifier.kind() == "import_specifier"
                && specifier.child_by_field_name("alias").is_none()
                && specifier.child_by_field_name("name").is_some_and(|name| {
                    name.kind() == "identifier"
                        && plain_name(name, source).is_some_and(|name| names.insert(name))
                })
                && node_signature_tokens(specifier, source)
                    .is_some_and(|tokens| tokens.0.len() == 1)
        })
        && !node_signature_tokens(node, source)
            .is_some_and(|tokens| tokens.0.iter().any(|(kind, _)| kind == "type"))
}

fn exported_names(file: &SourceFile, kind: &str) -> Vec<String> {
    children(file.root())
        .into_iter()
        .filter_map(|statement| {
            let declaration = statement.child_by_field_name("declaration")?;
            (statement.kind() == "export_statement" && declaration.kind() == kind).then(|| {
                declaration
                    .child_by_field_name("name")
                    .and_then(|name| plain_name(name, &file.source))
            })?
        })
        .collect()
}

fn exported<'a>(file: &'a SourceFile, name: &str, kind: &str) -> Option<(Node<'a>, Node<'a>)> {
    let mut found = None;
    for statement in children(file.root()) {
        let declaration = statement
            .child_by_field_name("declaration")
            .unwrap_or(statement);
        if declaration
            .child_by_field_name("name")
            .and_then(|node| text(node, &file.source))
            != Some(name)
        {
            continue;
        }
        if found.is_some() || statement.kind() != "export_statement" || declaration.kind() != kind {
            return None;
        }
        let prefix = signature_tokens_in_range(
            statement,
            &file.source,
            statement.start_byte(),
            declaration.start_byte(),
        )?;
        if prefix.0 != vec![("export".to_owned(), "export".to_owned())] {
            return None;
        }
        found = Some((declaration, statement));
    }
    let found = found?;
    let bindings = super::ts_const_arg::collect_bindings_named(file.root(), &file.source, name);
    (bindings.len() == 1 && bindings[0].id() == found.0.id()).then_some(found)
}

fn parameters(node: Node<'_>, source: &[u8]) -> Option<Vec<ContractType>> {
    children(node)
        .into_iter()
        .map(|parameter| {
            if parameter.kind() != "required_parameter"
                || parameter.child_by_field_name("value").is_some()
            {
                return None;
            }
            let pattern = parameter.child_by_field_name("pattern")?;
            if pattern.kind() != "identifier" || plain_name(pattern, source).is_none() {
                return None;
            }
            let ty = ContractType::read(parameter.child_by_field_name("type")?, source, 0)?;
            (ty != ContractType::Void && children(parameter).len() == 2).then_some(ty)
        })
        .collect()
}

struct FunctionContract<'a> {
    name: Node<'a>,
    parameters: Vec<ContractType>,
    result: ContractType,
    before_return: SigTokens,
    after_return: SigTokens,
}

fn function<'a>(file: &'a SourceFile, name: &str) -> Option<FunctionContract<'a>> {
    let (node, statement) = exported(file, name, "function_declaration")?;
    if node.child_by_field_name("type_parameters").is_some() {
        return None;
    }
    let body = node.child_by_field_name("body")?;
    let params = node.child_by_field_name("parameters")?;
    let result = node.child_by_field_name("return_type")?;
    let prefix =
        signature_tokens_in_range(node, &file.source, node.start_byte(), params.start_byte())?;
    if prefix.0.len() != 2 || prefix.0[0].0 != "function" || prefix.0[1].0 != "identifier" {
        return None;
    }
    Some(FunctionContract {
        name: node.child_by_field_name("name")?,
        parameters: parameters(params, &file.source)?,
        result: ContractType::read(result, &file.source, 0)?,
        before_return: signature_tokens_in_range(
            statement,
            &file.source,
            statement.start_byte(),
            result.start_byte(),
        )?,
        after_return: signature_tokens_in_range(
            statement,
            &file.source,
            result.end_byte(),
            body.start_byte(),
        )?,
    })
}

struct Slot<'a> {
    declaration_name: Node<'a>,
    name: String,
    parameters: Vec<ContractType>,
    result: ContractType,
    before_return: SigTokens,
    after_return: SigTokens,
}

fn slot<'a>(file: &'a SourceFile, name: &str) -> Option<Slot<'a>> {
    let (declaration, statement) = exported(file, name, "interface_declaration")?;
    let body = declaration.child_by_field_name("body")?;
    if children(declaration).len() != 2 {
        return None;
    }
    let property = only_child(body).filter(|node| node.kind() == "property_signature")?;
    let property_name = property.child_by_field_name("name")?;
    if property_name.kind() != "property_identifier" {
        return None;
    }
    let signature = only_child(property.child_by_field_name("type")?)?;
    if signature.kind() != "function_type" {
        return None;
    }
    if signature.child_by_field_name("type_parameters").is_some() {
        return None;
    }
    let result = signature.child_by_field_name("return_type")?;
    let tokens = node_signature_tokens(property, &file.source)?;
    if tokens.0.iter().any(|(kind, _)| kind == "?") {
        return None;
    }
    Some(Slot {
        declaration_name: declaration.child_by_field_name("name")?,
        name: plain_name(property_name, &file.source)?,
        parameters: parameters(signature.child_by_field_name("parameters")?, &file.source)?,
        result: ContractType::read(result, &file.source, 0)?,
        before_return: signature_tokens_in_range(
            statement,
            &file.source,
            statement.start_byte(),
            result.start_byte(),
        )?,
        after_return: signature_tokens_in_range(
            statement,
            &file.source,
            result.end_byte(),
            statement.end_byte(),
        )?,
    })
}

fn slot_pair<'a>(pair: &'a FilePair, name: &str) -> Option<(Slot<'a>, Slot<'a>)> {
    let old = slot(&pair.old, name)?;
    let new = slot(&pair.new, name)?;
    (old.name == new.name
        && old.parameters == new.parameters
        && old.result != new.result
        && old.before_return == new.before_return
        && old.after_return == new.after_return)
        .then_some((old, new))
}

fn producer_contract_pair<'a>(
    pair: &'a FilePair,
    name: &str,
    old_slot: &Slot<'_>,
    new_slot: &Slot<'_>,
) -> Option<(FunctionContract<'a>, FunctionContract<'a>)> {
    let old = function(&pair.old, name)?;
    let new = function(&pair.new, name)?;
    (old.before_return == new.before_return
        && old.after_return == new.after_return
        && old.result != new.result
        && old.parameters == old_slot.parameters
        && new.parameters == new_slot.parameters
        && old_slot.result.accepts(&old.result)
        && new_slot.result.accepts(&new.result))
    .then_some((old, new))
}

struct Consumer<'a> {
    name: String,
    old_name: Node<'a>,
    new_name: Node<'a>,
    old_type: Node<'a>,
    new_type: Node<'a>,
}

fn consumer_pair<'a>(
    pair: &'a FilePair,
    interface: &str,
    old_slot: &Slot<'_>,
    new_slot: &Slot<'_>,
) -> Option<Consumer<'a>> {
    let mut found = None;
    for name in exported_names(&pair.new, "function_declaration") {
        let old = consumer_contract(&pair.old, &name, interface, old_slot);
        let new = consumer_contract(&pair.new, &name, interface, new_slot);
        let (Some(old), Some(new)) = (old, new) else {
            continue;
        };
        if found.is_some() || old.2 != new.2 {
            return None;
        }
        found = Some(Consumer {
            name,
            old_name: old.0,
            new_name: new.0,
            old_type: old.1,
            new_type: new.1,
        });
    }
    found
}

fn consumer_contract<'a>(
    file: &'a SourceFile,
    name: &str,
    interface: &str,
    slot: &Slot<'_>,
) -> Option<(Node<'a>, Node<'a>, SigTokens)> {
    let (node, statement) = exported(file, name, "function_declaration")?;
    if node.child_by_field_name("type_parameters").is_some() {
        return None;
    }
    let params = node.child_by_field_name("parameters")?;
    let parameter = only_child(params).filter(|node| node.kind() == "required_parameter")?;
    if parameter.child_by_field_name("value").is_some() || children(parameter).len() != 2 {
        return None;
    }
    let pattern = parameter.child_by_field_name("pattern")?;
    if pattern.kind() != "identifier" {
        return None;
    }
    let parameter_name = plain_name(pattern, &file.source)?;
    let type_name = only_child(parameter.child_by_field_name("type")?)?;
    if type_name.kind() != "type_identifier" || text(type_name, &file.source)? != interface {
        return None;
    }
    let prefix =
        signature_tokens_in_range(node, &file.source, node.start_byte(), params.start_byte())?;
    if prefix.0.len() != 2 || prefix.0[0].0 != "function" || prefix.0[1].0 != "identifier" {
        return None;
    }
    let result = ContractType::read(node.child_by_field_name("return_type")?, &file.source, 0)?;
    if matches!(result, ContractType::Array { .. }) {
        return None;
    }
    let body = node.child_by_field_name("body")?;
    if !bounded_body(body, &file.source, &parameter_name, slot, &result) {
        return None;
    }
    Some((
        node.child_by_field_name("name")?,
        type_name,
        signature_tokens_in_range(
            statement,
            &file.source,
            statement.start_byte(),
            body.start_byte(),
        )?,
    ))
}

fn bounded_body(
    body: Node<'_>,
    source: &[u8],
    parameter: &str,
    slot: &Slot<'_>,
    result: &ContractType,
) -> bool {
    let statements = children(body);
    let mut returned = false;
    for (index, statement) in statements.iter().enumerate() {
        let Some(expression) = only_child(*statement) else {
            return false;
        };
        match statement.kind() {
            "expression_statement" if callback_call(expression, source, parameter, slot) => {}
            "return_statement" if index + 1 == statements.len() => {
                returned = literal_matches(expression, source, result)
                    || (result == &ContractType::Primitive("number".to_owned())
                        && expression.kind() == "member_expression"
                        && expression
                            .child_by_field_name("property")
                            .and_then(|node| text(node, source))
                            == Some("length")
                        && expression
                            .child_by_field_name("object")
                            .is_some_and(|call| callback_call(call, source, parameter, slot))
                        && matches!(slot.result, ContractType::Array { .. }));
                if !returned {
                    return false;
                }
            }
            _ => return false,
        }
    }
    returned || (result == &ContractType::Void && !statements.is_empty())
}

fn callback_call(node: Node<'_>, source: &[u8], parameter: &str, slot: &Slot<'_>) -> bool {
    let Some(callee) = ordinary_call(node) else {
        return false;
    };
    if callee.kind() != "member_expression" || children(callee).len() != 2 {
        return false;
    }
    if callee
        .child_by_field_name("object")
        .is_none_or(|node| node.kind() != "identifier" || text(node, source) != Some(parameter))
        || callee
            .child_by_field_name("property")
            .is_none_or(|node| text(node, source) != Some(slot.name.as_str()))
    {
        return false;
    }
    let Some(arguments) = node.child_by_field_name("arguments") else {
        return false;
    };
    let arguments = children(arguments);
    arguments.len() == slot.parameters.len()
        && arguments
            .iter()
            .zip(&slot.parameters)
            .all(|(argument, ty)| literal_matches(*argument, source, ty))
}

fn literal_matches(node: Node<'_>, source: &[u8], expected: &ContractType) -> bool {
    matches!((node.kind(), expected),
        ("string", ContractType::Primitive(name)) if name == "string" && static_specifier(node, source).is_some())
        || matches!((node.kind(), expected), ("number", ContractType::Primitive(name)) if name == if text(node, source).is_some_and(|text| text.ends_with('n')) { "bigint" } else { "number" })
        || matches!((node.kind(), expected), ("true" | "false", ContractType::Primitive(name)) if name == "boolean")
}

fn ordinary_call(node: Node<'_>) -> Option<Node<'_>> {
    if node.kind() != "call_expression" || node.child_by_field_name("type_arguments").is_some() {
        return None;
    }
    let mut cursor = node.walk();
    if node
        .children(&mut cursor)
        .any(|node| matches!(node.kind(), "?." | "optional_chain"))
    {
        return None;
    }
    node.child_by_field_name("function")
}

struct Scope<'a> {
    producer_path: &'a str,
    producer: &'a str,
    interface_path: &'a str,
    interface: &'a str,
    consumer: &'a str,
    property: &'a str,
}

fn position(path: &str, node: Node<'_>) -> Position {
    (path.to_owned(), node.start_byte(), node.end_byte())
}

fn prove_side(
    inventory: &Inventory,
    new_side: bool,
    scope: &Scope<'_>,
    producer_name: Node<'_>,
    interface_name: Node<'_>,
    consumer_name: Node<'_>,
    consumer_type: Node<'_>,
) -> Option<Vec<(String, String)>> {
    let mut allowed = BTreeSet::from([
        position(scope.producer_path, producer_name),
        position(scope.interface_path, interface_name),
        position(scope.interface_path, consumer_name),
        position(scope.interface_path, consumer_type),
    ]);
    for (path, pair) in &inventory.files {
        let file = if new_side { &pair.new } else { &pair.old };
        for statement in children(file.root())
            .into_iter()
            .filter(|node| node.kind() == "import_statement")
        {
            let source = static_specifier(statement.child_by_field_name("source")?, &file.source)?;
            let candidates = resolve_relative_specifier(path, source, &inventory.paths);
            let [target] = candidates.as_slice() else {
                return None;
            };
            for node in descendants(statement) {
                if node.kind() != "import_specifier" {
                    continue;
                }
                let name_node = node.child_by_field_name("name")?;
                let name = text(name_node, &file.source)?;
                let expected = if name == scope.producer {
                    scope.producer_path
                } else if name == scope.consumer || name == scope.interface {
                    scope.interface_path
                } else {
                    continue;
                };
                if target != expected || !single_binding(file, name, statement) {
                    return None;
                }
                allowed.insert(position(path, name_node));
            }
        }
    }
    let mut calls = Vec::new();
    for (path, pair) in &inventory.files {
        let file = if new_side { &pair.new } else { &pair.old };
        for &(start, end) in file.occurrences.get(scope.producer).into_iter().flatten() {
            if allowed.contains(&(path.clone(), start, end)) {
                continue;
            }
            let identifier = file.at(start, end)?;
            let pair = identifier.parent().filter(|node| node.kind() == "pair")?;
            if pair.child_by_field_name("value")?.id() != identifier.id()
                || pair.child_by_field_name("key")?.kind() != "property_identifier"
                || text(pair.child_by_field_name("key")?, &file.source)? != scope.property
            {
                return None;
            }
            let object = pair.parent().filter(|node| node.kind() == "object")?;
            if only_child(object)?.id() != pair.id() {
                return None;
            }
            let arguments = object.parent().filter(|node| node.kind() == "arguments")?;
            if only_child(arguments)?.id() != object.id() {
                return None;
            }
            let call = arguments.parent()?;
            let callee = ordinary_call(call)?;
            if callee.kind() != "identifier"
                || text(callee, &file.source)? != scope.consumer
                || !binding_owns(
                    inventory,
                    file,
                    path,
                    identifier,
                    scope.producer,
                    scope.producer_path,
                )
                || !binding_owns(
                    inventory,
                    file,
                    path,
                    callee,
                    scope.consumer,
                    scope.interface_path,
                )
            {
                return None;
            }
            allowed.insert(position(path, identifier));
            allowed.insert(position(path, callee));
            // 原文と個数を旧新で照合し、変更後に新設した手渡しを証拠にしない。
            calls.push((path.clone(), text(call, &file.source)?.to_owned()));
        }
    }
    if calls.is_empty() {
        return None;
    }
    for (path, pair) in &inventory.files {
        let file = if new_side { &pair.new } else { &pair.old };
        for name in [scope.producer, scope.interface, scope.consumer] {
            for &(start, end) in file.occurrences.get(name).into_iter().flatten() {
                if !allowed.contains(&(path.clone(), start, end)) {
                    return None;
                }
            }
        }
    }
    calls.sort();
    Some(calls)
}

fn single_binding(file: &SourceFile, name: &str, expected: Node<'_>) -> bool {
    let bindings = super::ts_const_arg::collect_bindings_named(file.root(), &file.source, name);
    !bindings.is_empty()
        && bindings.iter().all(|binding| {
            binding.id() == expected.id()
                || (expected.kind() == "import_statement"
                    && binding.kind() == "import_specifier"
                    && binding.start_byte() >= expected.start_byte()
                    && binding.end_byte() <= expected.end_byte())
        })
}

fn binding_owns(
    inventory: &Inventory,
    file: &SourceFile,
    path: &str,
    node: Node<'_>,
    name: &str,
    owner: &str,
) -> bool {
    use crate::engine::js_scope::{Namespace, ScopeBinding, resolve_binding_scope};
    let Some((scope, binding)) = resolve_binding_scope(node, &file.source, name, Namespace::Value)
    else {
        return false;
    };
    if scope.kind() != "program" {
        return false;
    }
    match binding {
        ScopeBinding::FunctionDeclaration => {
            path == owner && exported(file, name, "function_declaration").is_some()
        }
        ScopeBinding::Import(import) => {
            let Some(specifier) = import
                .child_by_field_name("source")
                .and_then(|node| static_specifier(node, &file.source))
            else {
                return false;
            };
            let targets = resolve_relative_specifier(path, specifier, &inventory.paths);
            single_binding(file, name, import) && targets.as_slice() == [owner]
        }
        _ => false,
    }
}

fn descendants(root: Node<'_>) -> Vec<Node<'_>> {
    let mut result = Vec::new();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        result.push(node);
        pending.extend(children(node));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(source: &str) -> SourceFile {
        SourceFile::parse(source.as_bytes().to_vec(), LangId::Typescript, &mut 0).unwrap()
    }

    fn return_type(annotation: &str) -> Option<ContractType> {
        let file = parsed(&format!("export function f(): {annotation} {{ throw 0; }}"));
        function(&file, "f").map(|function| function.result)
    }

    #[test]
    fn function_value_handoff_closed_return_types_preserve_readonly_direction() {
        for (value, target, accepted) in [
            ("void", "void", true),
            ("string[]", "readonly string[]", true),
            ("readonly string[]", "string[]", false),
            ("number[]", "readonly string[]", false),
            (
                "readonly string[][]",
                "readonly (readonly string[])[]",
                true,
            ),
            (
                "readonly (readonly string[])[]",
                "readonly string[][]",
                false,
            ),
            ("string", "number", false),
            ("string[]", "void", false),
        ] {
            assert_eq!(
                return_type(target)
                    .unwrap()
                    .accepts(&return_type(value).unwrap()),
                accepted,
                "{value} -> {target}"
            );
        }
        for annotation in [
            "any",
            "unknown",
            "Named",
            "string | number",
            "[string]",
            "Array<string>",
            "void[]",
        ] {
            assert!(return_type(annotation).is_none(), "{annotation}");
        }
    }

    #[test]
    fn function_value_handoff_requires_independent_old_and_new_assignments() {
        let old_slot = parsed("export interface Handlers { clone: (name: string) => void; }");
        let new_slot =
            parsed("export interface Handlers { clone: (name: string) => readonly string[]; }");
        let old_slot = slot(&old_slot, "Handlers").unwrap();
        let new_slot = slot(&new_slot, "Handlers").unwrap();
        for (old, new, accepted) in [
            ("void", "string[]", true),
            ("number", "string[]", false),
            ("void", "number[]", false),
            ("void", "readonly string[]", true),
        ] {
            let pair = FilePair {
                old: parsed(&format!(
                    "export function cloneText(name: string): {old} {{ throw 0; }}"
                )),
                new: parsed(&format!(
                    "export function cloneText(name: string): {new} {{ throw 0; }}"
                )),
            };
            assert_eq!(
                producer_contract_pair(&pair, "cloneText", &old_slot, &new_slot).is_some(),
                accepted,
                "{old} -> {new}"
            );
        }
    }

    #[test]
    fn function_value_handoff_rejects_bad_syntax_and_explicit_inventory_limits() {
        assert!(
            SourceFile::parse(b"export function f( {".to_vec(), LangId::Typescript, &mut 0)
                .is_none()
        );
        assert!(
            SourceFile::parse(
                b"export function f(): number { return 0;".to_vec(),
                LangId::Typescript,
                &mut 0
            )
            .is_none()
        );
        let mut nodes = MAX_NODES;
        assert!(
            SourceFile::parse(b"const x = 1;".to_vec(), LangId::Typescript, &mut nodes).is_none()
        );
        let source = format!(
            "const value = {}0{};",
            "(".repeat(MAX_DEPTH + 1),
            ")".repeat(MAX_DEPTH + 1)
        );
        assert!(SourceFile::parse(source.into_bytes(), LangId::Typescript, &mut 0).is_none());
        let tree = parser::parse_source(
            b"export function f(name: string): string[];",
            LangId::Typescript,
        )
        .unwrap();
        assert!(!tree.root_node().has_error());
    }

    #[test]
    fn function_value_handoff_node_contracts_exist_in_both_grammars() {
        for lang in [LangId::Typescript, LangId::Tsx] {
            for kind in [
                "type_annotation",
                "parenthesized_type",
                "predefined_type",
                "array_type",
                "readonly_type",
                "function_declaration",
                "interface_declaration",
                "property_signature",
                "function_type",
                "required_parameter",
                "pair",
                "object",
                "arguments",
                "call_expression",
                "member_expression",
                "identifier",
                "type_identifier",
                "property_identifier",
                "import_specifier",
                "import_clause",
                "named_imports",
                "import_statement",
                "export_statement",
                "subscript_expression",
                "import_alias",
                "internal_module",
                "ambient_declaration",
            ] {
                tree_sitter::Query::new(&lang.ts_language(), &format!("({kind}) @node"))
                    .unwrap_or_else(|error| panic!("{lang:?} {kind}: {error}"));
            }
        }
    }
}
