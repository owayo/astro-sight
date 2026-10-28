//! 削除時の配列 producer と `.map(consumer)` を、両方の参照監査で同じ証拠へ結ぶ。

use super::*;

pub(super) struct WrappedProducer {
    pub(super) path: String,
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) promise: bool,
}

pub(super) struct RowArrayMapSite<'t> {
    producer_call: Node<'t>,
    pub(super) consumer: Node<'t>,
}

impl Prover<'_, '_> {
    pub(super) fn scalar_removal(&self) -> bool {
        matches!(&self.delta.mode, MemberChangeMode::Removal(removed)
            if removed.invariant_properties.is_none())
    }

    pub(super) fn row_array_map_site<'t>(
        &self,
        map_call: Node<'t>,
        parsed: &ParsedRefFile,
        path: &str,
        caches: &mut ApiClosureCaches,
    ) -> Option<RowArrayMapSite<'t>> {
        if !self.scalar_removal() {
            return None;
        }
        let (receiver, consumer) = consumer_map_parts(map_call, &parsed.source)?;
        let (producer_call, awaited) = producer_receiver(receiver)?;
        if self.registered_wrapped_call(producer_call, parsed, path, caches)? != awaited {
            return None;
        }
        let name = consumer.utf8_text(&parsed.source).ok()?;
        self.consumers
            .iter()
            .any(|site| {
                site.index == 0
                    && site.function == name
                    && self.callee_is_consumer(consumer, parsed, path, site, caches)
                    && self.array_map_consumer_is_plain(site, path, caches)
            })
            .then_some(RowArrayMapSite {
                producer_call,
                consumer,
            })
    }

    pub(super) fn wrapped_call_is_consumed(
        &self,
        call: Node<'_>,
        parsed: &ParsedRefFile,
        path: &str,
        caches: &mut ApiClosureCaches,
    ) -> bool {
        let Some(map_call) = containing_map_call(call) else {
            return false;
        };
        self.row_array_map_site(map_call, parsed, path, caches)
            .is_some_and(|site| {
                site.producer_call.start_byte() == call.start_byte()
                    && site.producer_call.end_byte() == call.end_byte()
            })
    }

    fn registered_wrapped_call(
        &self,
        call: Node<'_>,
        parsed: &ParsedRefFile,
        path: &str,
        caches: &mut ApiClosureCaches,
    ) -> Option<bool> {
        if !ordinary_call(call) {
            return None;
        }
        let callee = call
            .child_by_field_name("function")
            .filter(|node| node.kind() == "identifier")?;
        let name = callee.utf8_text(&parsed.source).ok()?;
        match resolve_binding_scope(callee, &parsed.source, name, Namespace::Value)? {
            (scope, ScopeBinding::FunctionDeclaration) if scope.kind() == "program" => {
                self.registered_wrapped_at(parsed, path, name, false)
            }
            (_, ScopeBinding::Import(statement)) => {
                let target = self.import_target(
                    statement,
                    &parsed.source,
                    path,
                    name,
                    ImportUse::Value,
                    caches,
                )?;
                let file = parsed_ref_file_shared(self.scope.dir, &target, caches)?;
                is_checked_source(&file, &target).then_some(())?;
                self.registered_wrapped_at(&file, &target, name, true)
            }
            _ => None,
        }
    }

    fn registered_wrapped_at(
        &self,
        parsed: &ParsedRefFile,
        path: &str,
        name: &str,
        require_export: bool,
    ) -> Option<bool> {
        let function = unique_root_function(parsed, name, require_export)?;
        self.wrapped_producers
            .iter()
            .find(|producer| {
                self.same_path(&producer.path, path)
                    && producer.start == function.start_byte()
                    && producer.end == function.end_byte()
            })
            .map(|producer| producer.promise)
    }

    fn array_map_consumer_is_plain(
        &self,
        site: &ConsumerSite,
        use_path: &str,
        caches: &mut ApiClosureCaches,
    ) -> bool {
        let Some(parsed) = parsed_ref_file_shared(self.scope.dir, &site.path, caches) else {
            return false;
        };
        let Some(function) = unique_root_function(
            &parsed,
            &site.function,
            !self.same_path(&site.path, use_path),
        ) else {
            return false;
        };
        if has_token(function, "async")
            || has_token(function, "*")
            || function.child_by_field_name("type_parameters").is_some()
        {
            return false;
        }
        let Some(parameters) = function.child_by_field_name("parameters") else {
            return false;
        };
        let Some(parameter) = only_named_child(parameters) else {
            return false;
        };
        if parameter.kind() != "required_parameter"
            || parameter.child_by_field_name("value").is_some()
            || parameter
                .child_by_field_name("pattern")
                .is_none_or(|node| node.kind() != "identifier")
            || parameter
                .child_by_field_name("type")
                .is_none_or(|annotation| {
                    !self.annotation_is_target(annotation, &parsed, &site.path, caches)
                })
        {
            return false;
        }
        function
            .child_by_field_name("return_type")
            .and_then(only_named_child)
            .is_some_and(|ty| primitive_result(ty, &parsed.source))
    }
}

fn unique_root_function<'t>(
    parsed: &'t ParsedRefFile,
    name: &str,
    require_export: bool,
) -> Option<Node<'t>> {
    let root = parsed.tree.root_node();
    if root.has_error() {
        return None;
    }
    let bindings = super::super::ts_const_arg::collect_bindings_named(root, &parsed.source, name);
    let [function] = bindings.as_slice() else {
        return None;
    };
    if function.kind() != "function_declaration" || !is_top_level_declaration(*function) {
        return None;
    }
    if require_export {
        let direct = function.parent().is_some_and(|parent| {
            parent.kind() == "export_statement" && !has_token(parent, "default")
        });
        let by_clause = any_node(root, Children::Named, |node| {
            node.kind() == "export_specifier"
                && node.child_by_field_name("name").is_some_and(|identifier| {
                    plain_export_specifier(identifier, &parsed.source)
                        && identifier.utf8_text(&parsed.source).ok() == Some(name)
                })
                && node
                    .parent()
                    .and_then(|clause| clause.parent())
                    .is_some_and(|statement| {
                        statement.kind() == "export_statement"
                            && statement.child_by_field_name("source").is_none()
                    })
        });
        if !direct && !by_clause {
            return None;
        }
    }
    Some(*function)
}

fn primitive_result(ty: Node<'_>, source: &[u8]) -> bool {
    (ty.kind() == "predefined_type" && ty.utf8_text(source).ok() == Some("void"))
        || simple_type(ty, source).is_some_and(|atoms| {
            atoms.iter().all(|atom| {
                !matches!(
                    atom,
                    TypeAtom::Array(_) | TypeAtom::Any | TypeAtom::Unknown | TypeAtom::StrLit(None)
                )
            })
        })
}

pub(super) fn ordinary_call(call: Node<'_>) -> bool {
    call.kind() == "call_expression"
        && call.child_by_field_name("optional_chain").is_none()
        // TS/TSX の optional call は field ではなく無名の `?.` トークンを持つ。
        && !has_token(call, "?.")
        && call.child_by_field_name("type_arguments").is_none()
        && call.child_by_field_name("arguments").is_some_and(|arguments| {
            arguments.kind() == "arguments" && !has_spread(arguments)
        })
}

fn has_spread(arguments: Node<'_>) -> bool {
    let mut cursor = arguments.walk();
    arguments
        .named_children(&mut cursor)
        .any(|node| node.kind() == "spread_element")
}

fn consumer_map_parts<'t>(call: Node<'t>, source: &[u8]) -> Option<(Node<'t>, Node<'t>)> {
    if !ordinary_call(call) {
        return None;
    }
    let member = call.child_by_field_name("function")?;
    if member.kind() != "member_expression"
        || member.child_by_field_name("optional_chain").is_some()
        || member.child_by_field_name("property")?.kind() != "property_identifier"
        || member
            .child_by_field_name("property")?
            .utf8_text(source)
            .ok()?
            != "map"
    {
        return None;
    }
    let consumer = only_named_child(call.child_by_field_name("arguments")?)?;
    (consumer.kind() == "identifier").then_some((member.child_by_field_name("object")?, consumer))
}

fn producer_receiver(receiver: Node<'_>) -> Option<(Node<'_>, bool)> {
    let receiver = unwrap_parens(receiver)?;
    let (call, awaited) = if receiver.kind() == "await_expression" {
        (unwrap_parens(only_named_child(receiver)?)?, true)
    } else {
        (receiver, false)
    };
    ordinary_call(call).then_some((call, awaited))
}

fn containing_map_call(mut producer: Node<'_>) -> Option<Node<'_>> {
    let mut awaited = false;
    for _ in 0..MAX_UNWRAP {
        let parent = producer.parent()?;
        match parent.kind() {
            "parenthesized_expression" => producer = parent,
            "await_expression" if !awaited => {
                awaited = true;
                producer = parent;
            }
            "member_expression" if parent.child_by_field_name("object")?.id() == producer.id() => {
                let call = parent.parent()?;
                return (call.kind() == "call_expression"
                    && call.child_by_field_name("function")?.id() == parent.id())
                .then_some(call);
            }
            _ => return None,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_array_map_nodes_exist_and_direct_shapes_match_both_grammars() {
        for lang in [LangId::Typescript, LangId::Tsx] {
            for query in [
                "(call_expression function: (_) arguments: (arguments))",
                "(call_expression type_arguments: (type_arguments))",
                "(call_expression \"?.\" @optional)",
                "(member_expression object: (_) property: (property_identifier))",
                "(optional_chain)",
                "(await_expression)",
                "(parenthesized_expression)",
                "(spread_element)",
                "(export_specifier name: (_))",
                "(function_declaration parameters: (formal_parameters) return_type: (type_annotation))",
                "(required_parameter pattern: (identifier) type: (type_annotation))",
            ] {
                tree_sitter::Query::new(&lang.ts_language(), query).unwrap();
            }
            for (expression, expected) in [
                ("toRows(['a']).map(label)", Some(false)),
                ("(toRows(['a'])).map(/* comment */ label)", Some(false)),
                ("(await toRows(['a'])).map(label)", Some(true)),
                ("(await (toRows(['a']))).map(label)", Some(true)),
                ("toRows(['a']).forEach(label)", None),
                ("toRows(['a']).map(label, context)", None),
                ("toRows(['a']).map<string>(label)", None),
                ("toRows(['a'])?.map(label)", None),
                ("toRows?.(['a']).map(label)", None),
                ("toRows(['a']).map?.(label)", None),
                ("(await toRows?.(['a'])).map(label)", None),
                ("toRows<string>(['a']).map(label)", None),
                ("toRows(...values).map(label)", None),
                ("toRows(['a']).map(row => label(row))", None),
                ("toRows(['a']).map(...label)", None),
                ("(await toRows(['a']) ?? []).map(label)", None),
                ("(await toRows(['a']))!.map(label)", None),
                ("toRows(['a']).then(rows => rows.map(label))", None),
            ] {
                let source = format!("{expression};");
                let tree = crate::engine::parser::parse_source(source.as_bytes(), lang).unwrap();
                assert!(!tree.root_node().has_error(), "{source}");
                let call = only_named_child(only_named_child(tree.root_node()).unwrap()).unwrap();
                let shape =
                    consumer_map_parts(call, source.as_bytes()).and_then(|(receiver, consumer)| {
                        assert_eq!(consumer.utf8_text(source.as_bytes()).unwrap(), "label");
                        let (producer, awaited) = producer_receiver(receiver)?;
                        assert_eq!(containing_map_call(producer).unwrap().id(), call.id());
                        Some(awaited)
                    });
                assert_eq!(shape, expected, "{source}");
            }
        }
    }

    #[test]
    fn row_array_map_consumer_result_contains_no_row_value() {
        for lang in [LangId::Typescript, LangId::Tsx] {
            for (result, expected) in [
                ("string", true),
                ("number | undefined", true),
                ("boolean", true),
                ("void", true),
                ("string[]", false),
                ("any", false),
                ("unknown", false),
                ("Row", false),
                ("Row | null", false),
                ("Promise<string>", false),
            ] {
                let source = format!("function label(row: Row): {result} {{ return value; }}");
                let tree = crate::engine::parser::parse_source(source.as_bytes(), lang).unwrap();
                assert!(!tree.root_node().has_error(), "{source}");
                let function = only_named_child(tree.root_node()).unwrap();
                let result =
                    only_named_child(function.child_by_field_name("return_type").unwrap()).unwrap();
                assert_eq!(
                    primitive_result(result, source.as_bytes()),
                    expected,
                    "{source}"
                );
            }
        }
    }
}
