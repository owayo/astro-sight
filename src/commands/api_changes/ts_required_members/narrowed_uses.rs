//! 型縮小時は、値の作成だけでなく観測できる利用も確かめる (Issue #139)。

use super::*;

pub(super) fn known_type(atoms: &[TypeAtom]) -> bool {
    atoms.iter().all(|atom| match atom {
        TypeAtom::Any | TypeAtom::Unknown => false,
        TypeAtom::Array(items) => known_type(items),
        _ => true,
    })
}

impl Prover<'_, '_> {
    /// 型注釈だけで初期化子の any / assertion を隠さない。先行 const の直接の連鎖だけを追う。
    pub(super) fn narrowed_value_sources(
        &self,
        expr: Node<'_>,
        parsed: &ParsedRefFile,
        depth: usize,
        seen: &mut HashSet<usize>,
    ) -> bool {
        if depth >= 16
            || any_node(expr, Children::Named, |n| {
                matches!(
                    n.kind(),
                    "as_expression" | "type_assertion" | "non_null_expression"
                )
            })
        {
            return false;
        }
        let Some(value_type) = self.value_type(expr, parsed, 0).filter(|ty| known_type(ty)) else {
            return false;
        };
        let Some(value) = unwrap_parens(expr) else {
            return false;
        };
        if !matches!(value.kind(), "identifier" | "shorthand_property_identifier") {
            return true;
        }
        let Ok(name) = value.utf8_text(&parsed.source) else {
            return false;
        };
        match resolve_binding_scope(value, &parsed.source, name, Namespace::Value) {
            Some((_, ScopeBinding::Variable(declaration))) => {
                if !super::super::ts_const_arg::declarator_is_const(declaration)
                    || declaration.end_byte() > expr.start_byte()
                    || !seen.insert(declaration.id())
                {
                    return false;
                }
                declaration
                    .child_by_field_name("value")
                    .is_some_and(|init| {
                        self.value_type(init, parsed, 0)
                            .is_some_and(|ty| assignable(&ty, &value_type))
                            && self.narrowed_value_sources(init, parsed, depth + 1, seen)
                    })
            }
            Some((scope, ScopeBinding::Other)) => {
                let Some(param) = parameter_named(scope, &parsed.source, name) else {
                    return false;
                };
                let Some(declared_type) = param
                    .child_by_field_name("type")
                    .and_then(only_named_child)
                    .and_then(|ty| simple_type(ty, &parsed.source))
                else {
                    return false;
                };
                scope.child_by_field_name("body").is_some_and(|body| {
                    !return_producers::callback_rebinds(body, &parsed.source, name)
                }) && param.child_by_field_name("value").is_none_or(|init| {
                    self.value_type(init, parsed, 0)
                        .is_some_and(|ty| assignable(&ty, &declared_type))
                        && self.narrowed_value_sources(init, parsed, depth + 1, seen)
                })
            }
            _ => false,
        }
    }

    pub(super) fn prove_narrowed_uses(&self, caches: &mut ApiClosureCaches) -> bool {
        let mut paths = self.narrowed_paths.clone();
        for name in self.typeof_names.keys() {
            let Some(refs) = name_refs(caches, name) else {
                return false;
            };
            for r in refs.iter() {
                paths.insert(r.path.clone());
                if r.kind == Some(RefKind::Definition) {
                    continue;
                }
                match ref_import_role(self.scope.dir, r, name, caches) {
                    Some(RefImportRole::Plain) => continue,
                    Some(RefImportRole::Outside) => {}
                    _ => return false,
                }
                if ref_is_pass_through_reexport(self.scope.dir, r, name, caches) {
                    continue;
                }
                let Some(parsed) = parsed_ref_file_shared(self.scope.dir, &r.path, caches) else {
                    return false;
                };
                let Some(node) = ref_name_node(&parsed, name, r.line, r.column) else {
                    return false;
                };
                if plain_export_specifier(node, &parsed.source) {
                    continue;
                }
                if let Some(call) = direct_call(node) {
                    if !self.call_returns_target(call, &parsed, &r.path, caches)
                        || !self.narrowed_root_use(call, &parsed, &r.path, caches)
                    {
                        return false;
                    }
                } else if !self.identifier_has_target_type(node, &parsed, &r.path, caches) {
                    // producer を callback として渡す形や、名前空間越しの呼出しは解決しない。
                    return false;
                }
            }
        }
        for path in paths {
            let Some(parsed) = parsed_ref_file_shared(self.scope.dir, &path, caches) else {
                return false;
            };
            if !is_checked_source(&parsed, &path) {
                return false;
            }
            let result = walk_preorder(parsed.tree.root_node(), Children::Named, |node, _| {
                if node.kind() == "for_in_statement"
                    && self.loop_writes_narrowed_value(node, &parsed, &path, caches)
                {
                    return ControlFlow::Break(());
                }
                if node.kind() == "identifier"
                    && !is_binding_name(node)
                    && self.identifier_has_target_type(node, &parsed, &path, caches)
                    && !self.narrowed_root_use(node, &parsed, &path, caches)
                {
                    return ControlFlow::Break(());
                }
                ControlFlow::Continue(Walk::Descend)
            });
            if result.is_break() {
                return false;
            }
        }
        true
    }

    fn loop_writes_narrowed_value(
        &self,
        node: Node<'_>,
        parsed: &ParsedRefFile,
        path: &str,
        caches: &mut ApiClosureCaches,
    ) -> bool {
        if node
            .child_by_field_name("kind")
            .is_some_and(|kind| kind.kind() != "var")
        {
            return false;
        }
        let (Some(left), Some(outside)) = (node.child_by_field_name("left"), node.parent()) else {
            return true;
        };
        // for の左辺を新しい束縛とみなす scope 判定を通さず、代入先の外側の束縛を調べる。
        any_node(left, Children::Named, |ident| {
            if ident.kind() != "identifier" {
                return false;
            }
            let Ok(name) = ident.utf8_text(&parsed.source) else {
                return true;
            };
            let annotation =
                match resolve_binding_scope(outside, &parsed.source, name, Namespace::Value) {
                    Some((_, ScopeBinding::Variable(declaration))) => {
                        declaration.child_by_field_name("type")
                    }
                    Some((scope, ScopeBinding::Other)) => {
                        parameter_named(scope, &parsed.source, name)
                            .and_then(|param| param.child_by_field_name("type"))
                    }
                    _ => None,
                };
            annotation.is_some_and(|annotation| {
                self.annotation_is_target(annotation, parsed, path, caches)
            })
        })
    }

    fn narrowed_root_use(
        &self,
        mut value: Node<'_>,
        parsed: &ParsedRefFile,
        path: &str,
        caches: &mut ApiClosureCaches,
    ) -> bool {
        for _ in 0..MAX_UNWRAP {
            let Some(parent) = value.parent() else {
                return false;
            };
            if parent.kind() == "parenthesized_expression" {
                value = parent;
                continue;
            }
            let field_is =
                |field: &str| parent.child_by_field_name(field).map(|n| n.id()) == Some(value.id());
            return match parent.kind() {
                "member_expression" if field_is("object") => {
                    self.narrowed_member_use(parent, parsed)
                }
                "arguments" => self.argument_reaches_consumer(value, parent, parsed, path, caches),
                "return_statement" => {
                    let mut ancestor = parent.parent();
                    for _ in 0..MAX_UNWRAP {
                        let Some(node) = ancestor else {
                            return false;
                        };
                        if matches!(
                            node.kind(),
                            "function_declaration"
                                | "arrow_function"
                                | "function_expression"
                                | "method_definition"
                        ) {
                            return node.kind() == "function_declaration"
                                && is_top_level_declaration(node)
                                && node.child_by_field_name("return_type").is_some_and(
                                    |annotation| {
                                        self.annotation_is_target(annotation, parsed, path, caches)
                                    },
                                );
                        }
                        ancestor = node.parent();
                    }
                    false
                }
                "variable_declarator" if field_is("value") => {
                    super::super::ts_const_arg::declarator_is_const(parent)
                        && parent
                            .child_by_field_name("name")
                            .is_some_and(|n| n.kind() == "identifier")
                        && parent
                            .child_by_field_name("type")
                            .is_some_and(|annotation| {
                                self.annotation_is_target(annotation, parsed, path, caches)
                            })
                }
                "expression_statement" => true,
                // 別名・入れ物・文字列キー・assertion・未知の呼出しには値を流さない。
                _ => false,
            };
        }
        false
    }

    fn argument_reaches_consumer(
        &self,
        value: Node<'_>,
        arguments: Node<'_>,
        parsed: &ParsedRefFile,
        path: &str,
        caches: &mut ApiClosureCaches,
    ) -> bool {
        let Some(call) = arguments.parent().filter(|p| p.kind() == "call_expression") else {
            return false;
        };
        if call.child_by_field_name("optional_chain").is_some()
            || call.child_by_field_name("type_arguments").is_some()
        {
            return false;
        }
        let Some(callee) = call
            .child_by_field_name("function")
            .filter(|n| n.kind() == "identifier")
        else {
            return false;
        };
        let mut cursor = arguments.walk();
        let args: Vec<_> = arguments
            .named_children(&mut cursor)
            .filter(|n| n.kind() != "comment")
            .collect();
        if args.iter().any(|n| n.kind() == "spread_element") {
            return false;
        }
        let Some(index) = args.iter().position(|n| n.id() == value.id()) else {
            return false;
        };
        self.consumers.iter().any(|consumer| {
            consumer.index == index
                && callee.utf8_text(&parsed.source).ok() == Some(consumer.function.as_str())
                && self.callee_is_consumer(callee, parsed, path, consumer, caches)
        })
    }

    fn narrowed_member_use(&self, member: Node<'_>, parsed: &ParsedRefFile) -> bool {
        let source: &[u8] = &parsed.source;
        let Some(key) = member
            .child_by_field_name("property")
            .filter(|n| n.kind() == "property_identifier")
        else {
            return false;
        };
        let Ok(name) = key.utf8_text(source) else {
            return false;
        };
        if name.contains('\\') || member.child_by_field_name("optional_chain").is_some() {
            return false;
        }
        let Some(property) = self.delta.added.iter().find(|p| p.name == name) else {
            // メンバー関数は未知の書込みを行い得るので、変更していないキーでも呼び出さない。
            return !member.parent().is_some_and(|p| {
                p.kind() == "call_expression"
                    && p.child_by_field_name("function").map(|n| n.id()) == Some(member.id())
            });
        };
        if member_is_written(member) || binds_value_to_another_name(member, source, false) {
            return false;
        }
        let mut value = member;
        for _ in 0..MAX_UNWRAP {
            let Some(parent) = value.parent() else {
                return false;
            };
            if parent.kind() == "parenthesized_expression" {
                value = parent;
                continue;
            }
            return match parent.kind() {
                "binary_expression" => {
                    let Some(operator) = parent.child_by_field_name("operator") else {
                        return false;
                    };
                    if !matches!(
                        operator.kind(),
                        "==" | "!=" | "===" | "!==" | "<" | ">" | "<=" | ">="
                    ) {
                        return false;
                    }
                    let (Some(left), Some(right)) = (
                        parent.child_by_field_name("left"),
                        parent.child_by_field_name("right"),
                    ) else {
                        return false;
                    };
                    let other = if left.id() == value.id() {
                        right
                    } else if right.id() == value.id() {
                        left
                    } else {
                        return false;
                    };
                    self.value_type(other, parsed, 0).is_some_and(|ty| {
                        known_type(&ty)
                            && ty.iter().any(|atom| {
                                property.ty.iter().any(|target| {
                                    assignable(
                                        std::slice::from_ref(atom),
                                        std::slice::from_ref(target),
                                    ) || assignable(
                                        std::slice::from_ref(target),
                                        std::slice::from_ref(atom),
                                    )
                                })
                            })
                    })
                }
                "return_statement" => {
                    let mut ancestor = parent.parent();
                    for _ in 0..MAX_UNWRAP {
                        let Some(node) = ancestor else {
                            return false;
                        };
                        if matches!(
                            node.kind(),
                            "function_declaration"
                                | "arrow_function"
                                | "function_expression"
                                | "method_definition"
                        ) {
                            return node.kind() == "function_declaration"
                                && is_top_level_declaration(node)
                                && node.child_by_field_name("return_type").is_some();
                        }
                        ancestor = node.parent();
                    }
                    false
                }
                "template_substitution" | "expression_statement" => true,
                _ => false,
            };
        }
        false
    }
}

fn member_is_written(member: Node<'_>) -> bool {
    let mut current = member;
    for _ in 0..MAX_UNWRAP {
        let Some(parent) = current.parent() else {
            return false;
        };
        match parent.kind() {
            "assignment_expression" | "augmented_assignment_expression" => {
                return parent.child_by_field_name("left").is_some_and(|left| {
                    left.start_byte() <= member.start_byte() && left.end_byte() >= member.end_byte()
                });
            }
            "update_expression" => return true,
            "unary_expression"
                if parent
                    .child_by_field_name("operator")
                    .is_some_and(|op| op.kind() == "delete") =>
            {
                return true;
            }
            "statement_block" | "program" | "return_statement" | "expression_statement" => {
                return false;
            }
            _ => current = parent,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narrowing_guard_nodes_exist_in_typescript_grammars() {
        for lang in [LangId::Typescript, LangId::Tsx] {
            for kind in [
                "assignment_expression",
                "augmented_assignment_expression",
                "for_in_statement",
                "update_expression",
                "unary_expression",
                "member_expression",
                "non_null_expression",
                "as_expression",
                "object_pattern",
                "array_pattern",
            ] {
                assert!(
                    tree_sitter::Query::new(&lang.ts_language(), &format!("({kind}) @node"))
                        .is_ok(),
                    "{lang}: {kind}"
                );
            }
        }
        assert!(
            tree_sitter::Query::new(&LangId::Typescript.ts_language(), "(type_assertion) @node")
                .is_ok()
        );
    }
}
