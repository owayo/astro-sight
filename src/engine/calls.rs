use anyhow::Result;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Query, QueryCursor};

use crate::language::LangId;
use crate::models::call::{CallEdge, CallEndpoint, CallSite};
use crate::models::location::Range;

/// Extract call edges from a parsed tree.
/// If `filter_function` is provided, only return calls where the caller matches.
pub fn extract_calls(
    root: Node<'_>,
    source: &[u8],
    lang_id: LangId,
    filter_function: Option<&str>,
) -> Result<Vec<CallEdge>> {
    let query_src = call_query(lang_id);
    if query_src.is_empty() {
        return Ok(Vec::new());
    }

    let language = lang_id.ts_language();
    let query = Query::new(&language, query_src)?;
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, root, source);

    let mut edges = Vec::new();
    while let Some(m) = matches.next() {
        for capture in m.captures {
            let node = capture.node;
            let capture_name = &query.capture_names()[capture.index as usize];
            if !capture_name.ends_with("callee") {
                continue;
            }

            let callee_name = node.utf8_text(source).unwrap_or("").to_string();
            if callee_name.is_empty() {
                continue;
            }

            // Find the enclosing function (caller)
            let caller = find_enclosing_function(node, source, lang_id);
            let caller = match caller {
                Some(c) => c,
                None => continue, // top-level call, skip
            };

            if let Some(filter) = filter_function
                && caller.name != filter
            {
                continue;
            }

            // call_expression node is the parent of the callee identifier
            let call_node = node.parent().unwrap_or(node);
            let callee_range = Range::from(call_node.range());

            edges.push(CallEdge {
                caller,
                callee: CallEndpoint {
                    name: callee_name,
                    range: callee_range,
                },
                call_site: CallSite {
                    line: node.start_position().row,
                    column: node.start_position().column,
                },
            });
        }
    }

    Ok(edges)
}

/// Walk up the tree to find the nearest enclosing function definition.
fn find_enclosing_function(node: Node<'_>, source: &[u8], lang_id: LangId) -> Option<CallEndpoint> {
    let func_kinds = function_node_kinds(lang_id);
    let mut current = node.parent();

    while let Some(n) = current {
        if func_kinds.contains(&n.kind()) {
            let name = find_function_name(n, source, lang_id)?;
            return Some(CallEndpoint {
                name,
                range: Range::from(n.range()),
            });
        }
        current = n.parent();
    }

    None
}

/// Get function-like node kinds for a language.
fn function_node_kinds(lang_id: LangId) -> &'static [&'static str] {
    match lang_id {
        LangId::Rust => &["function_item"],
        LangId::C | LangId::Cpp => &["function_definition"],
        LangId::Python => &["function_definition"],
        LangId::Javascript => &[
            "function_declaration",
            "method_definition",
            "arrow_function",
        ],
        LangId::Typescript | LangId::Tsx => &[
            "function_declaration",
            "method_definition",
            "arrow_function",
        ],
        LangId::Go => &["function_declaration", "method_declaration"],
        LangId::Php => &["function_definition", "method_declaration"],
        LangId::Java => &["method_declaration", "constructor_declaration"],
        LangId::Kotlin => &["function_declaration"],
        LangId::Swift => &["function_declaration"],
        LangId::CSharp => &["method_declaration", "constructor_declaration"],
        LangId::Bash => &["function_definition"],
    }
}

/// Extract the name of a function node.
fn find_function_name(node: Node<'_>, source: &[u8], lang_id: LangId) -> Option<String> {
    // Try the "name" field first (most languages)
    if let Some(name_node) = node.child_by_field_name("name") {
        return name_node.utf8_text(source).ok().map(|s| s.to_string());
    }

    // Kotlin: function_declaration > simple_identifier
    if lang_id == LangId::Kotlin {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "simple_identifier" {
                return child.utf8_text(source).ok().map(|s| s.to_string());
            }
        }
    }

    // For function_definition in C/Rust: declarator > function_declarator > declarator (identifier)
    if let Some(decl) = node.child_by_field_name("declarator") {
        if let Some(inner) = decl.child_by_field_name("declarator") {
            return inner.utf8_text(source).ok().map(|s| s.to_string());
        }
        return decl.utf8_text(source).ok().map(|s| s.to_string());
    }

    // Fallback: first identifier child
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let k = child.kind();
        if k == "identifier" || k == "type_identifier" || k == "field_identifier" || k == "word" {
            return child.utf8_text(source).ok().map(|s| s.to_string());
        }
    }

    None
}

/// Language-specific tree-sitter queries for call expressions.
fn call_query(lang_id: LangId) -> &'static str {
    match lang_id {
        LangId::Rust => {
            r#"
            (call_expression function: (identifier) @direct.callee)
            (call_expression function: (field_expression field: (field_identifier) @method.callee))
            (call_expression function: (scoped_identifier name: (identifier) @scoped.callee))
            "#
        }
        LangId::C => {
            r#"
            (call_expression function: (identifier) @direct.callee)
            "#
        }
        LangId::Cpp => {
            r#"
            (call_expression function: (identifier) @direct.callee)
            (call_expression function: (field_expression field: (field_identifier) @method.callee))
            (call_expression function: (qualified_identifier name: (identifier) @scoped.callee))
            "#
        }
        LangId::Python => {
            r#"
            (call function: (identifier) @direct.callee)
            (call function: (attribute attribute: (identifier) @method.callee))
            "#
        }
        LangId::Javascript => {
            r#"
            (call_expression function: (identifier) @direct.callee)
            (call_expression function: (member_expression property: (property_identifier) @method.callee))
            "#
        }
        LangId::Typescript | LangId::Tsx => {
            r#"
            (call_expression function: (identifier) @direct.callee)
            (call_expression function: (member_expression property: (property_identifier) @method.callee))
            "#
        }
        LangId::Go => {
            r#"
            (call_expression function: (identifier) @direct.callee)
            (call_expression function: (selector_expression field: (field_identifier) @method.callee))
            "#
        }
        LangId::Php => {
            r#"
            (function_call_expression function: (name) @direct.callee)
            (member_call_expression name: (name) @method.callee)
            (scoped_call_expression name: (name) @scoped.callee)
            "#
        }
        LangId::Java => {
            r#"
            (method_invocation name: (identifier) @direct.callee)
            (method_invocation object: (identifier) name: (identifier) @method.callee)
            "#
        }
        LangId::Kotlin => {
            r#"
            (call_expression (simple_identifier) @direct.callee)
            (call_expression (navigation_expression (simple_identifier) @method.callee))
            "#
        }
        LangId::Swift => {
            r#"
            (call_expression (simple_identifier) @direct.callee)
            (call_expression
              (navigation_expression
                (navigation_suffix suffix: (simple_identifier) @method.callee)))
            "#
        }
        LangId::CSharp => {
            r#"
            (invocation_expression function: (identifier) @direct.callee)
            (invocation_expression function: (member_access_expression name: (identifier) @method.callee))
            "#
        }
        LangId::Bash => {
            r#"
            (command name: (command_name (word) @direct.callee))
            "#
        }
    }
}
