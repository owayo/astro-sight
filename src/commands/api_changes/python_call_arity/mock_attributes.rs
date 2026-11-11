//! 外へ渡さず設定・検査にだけ使う、由来の確かな Mock の属性を区別する。

use super::*;

pub(super) fn proves_configuration(
    attribute: Node<'_>,
    file: PyFile<'_>,
    dir: &str,
    caches: &mut ApiClosureCaches,
) -> bool {
    let Some(receiver) = attribute.child_by_field_name("object") else {
        return false;
    };
    if receiver.kind() != "identifier" || !configuration_use(receiver, file.source) {
        return false;
    }
    let Some(name) = node_text(receiver, file.source) else {
        return false;
    };
    let Some(function) = enclosing_function(receiver) else {
        return false;
    };
    let key = (file.path.to_owned(), function.id(), name.to_owned());
    if let Some(proven) = caches.python_classes.mock_receivers.get(&key) {
        return *proven;
    }
    let proven = prove_receiver(receiver, function, file, dir, name, caches);
    caches.python_classes.mock_receivers.insert(key, proven);
    proven
}

fn configuration_use(receiver: Node<'_>, source: &[u8]) -> bool {
    let Some(member) = receiver.parent().filter(|node| node.kind() == "attribute") else {
        return false;
    };
    if member.child_by_field_name("object").map(|n| n.id()) != Some(receiver.id()) {
        return false;
    }
    let Some(setting) = member.parent().filter(|node| node.kind() == "attribute") else {
        return false;
    };
    if setting.child_by_field_name("object").map(|n| n.id()) != Some(member.id()) {
        return false;
    }
    let Some(name) = setting
        .child_by_field_name("attribute")
        .and_then(|node| node_text(node, source))
    else {
        return false;
    };
    let Some(parent) = setting.parent() else {
        return false;
    };
    match name {
        "return_value" | "side_effect" => {
            parent.kind() == "assignment"
                && parent.child_by_field_name("left").map(|n| n.id()) == Some(setting.id())
        }
        "assert_called"
        | "assert_called_once"
        | "assert_called_with"
        | "assert_called_once_with"
        | "assert_any_call"
        | "assert_has_calls"
        | "assert_not_called" => {
            parent.kind() == "call"
                && parent.child_by_field_name("function").map(|n| n.id()) == Some(setting.id())
        }
        _ => false,
    }
}

fn prove_receiver(
    receiver: Node<'_>,
    function: Node<'_>,
    file: PyFile<'_>,
    dir: &str,
    name: &str,
    caches: &mut ApiClosureCaches,
) -> bool {
    let source = file.source;
    if parameter_named(function, source, name) || module_lookup_is_dynamic(file, caches) {
        return false;
    }
    let Some(body) = function.child_by_field_name("body") else {
        return false;
    };
    let mut cursor = body.walk();
    let assignments: Vec<_> = body
        .named_children(&mut cursor)
        .filter_map(|statement| {
            (statement.kind() == "expression_statement")
                .then(|| statement.named_child(0))
                .flatten()
        })
        .filter(|node| {
            node.kind() == "assignment"
                && node.end_byte() < receiver.start_byte()
                && node.child_by_field_name("left").is_some_and(|left| {
                    left.kind() == "identifier" && node_text(left, source) == Some(name)
                })
        })
        .collect();
    let [assignment] = assignments.as_slice() else {
        return false;
    };
    let Some(value) = assignment.child_by_field_name("right") else {
        return false;
    };
    let Some(factory) = value.child_by_field_name("function") else {
        return false;
    };
    if value.kind() != "call"
        || factory.kind() != "identifier"
        || value
            .child_by_field_name("arguments")
            .is_none_or(|arguments| {
                let mut cursor = arguments.walk();
                arguments
                    .named_children(&mut cursor)
                    .any(|node| node.kind() != "comment")
            })
        || !factory_is_imported(factory, file, dir, caches)
    {
        return false;
    }
    let binding = assignment.child_by_field_name("left").map(|node| node.id());
    let mut stack = vec![function];
    while let Some(node) = stack.pop() {
        if node.kind() == "identifier"
            && node_text(node, source) == Some(name)
            && Some(node.id()) != binding
            && (node.start_byte() < assignment.end_byte() || !configuration_use(node, source))
        {
            return false;
        }
        // 入れ子のスコープや動的書換えでは、束縛・属性の同一性を断定しない。
        if (node.id() != function.id()
            && matches!(
                node.kind(),
                "function_definition" | "class_definition" | "lambda"
            ))
            || (node.kind() == "identifier"
                && matches!(
                    node_text(node, source),
                    Some("exec" | "eval" | "globals" | "locals" | "vars" | "setattr" | "delattr")
                ))
        {
            return false;
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    true
}

fn factory_is_imported(
    factory: Node<'_>,
    file: PyFile<'_>,
    dir: &str,
    caches: &mut ApiClosureCaches,
) -> bool {
    let Some(name) = node_text(factory, file.source) else {
        return false;
    };
    if parameter_in_enclosing_scopes(factory, file.source, name) {
        return false;
    }
    let root = std::path::Path::new(dir);
    let mut directories = vec![root.to_path_buf(), root.join("src")];
    let caller = root.join(file.path);
    let mut parent = caller.parent();
    while let Some(directory) = parent.filter(|directory| directory.starts_with(root)) {
        directories.push(directory.to_path_buf());
        parent = directory.parent();
    }
    directories.sort_unstable();
    directories.dedup();
    if directories
        .iter()
        .any(|directory| shadows_unittest(directory))
    {
        return false;
    }
    let mut cursor = file.root.walk();
    file.root.named_children(&mut cursor).any(|statement| {
        if statement.kind() != "import_from_statement"
            || statement
                .child_by_field_name("module_name")
                .and_then(|module| node_text(module, file.source))
                != Some("unittest.mock")
            || has_other_root_binding(file, statement, name, caches)
        {
            return false;
        }
        let mut cursor = statement.walk();
        let imports: Vec<_> = statement
            .named_children(&mut cursor)
            .filter_map(|import| {
                let (imported, local) = if import.kind() == "aliased_import" {
                    (
                        import.child_by_field_name("name"),
                        import.child_by_field_name("alias"),
                    )
                } else if import.kind() == "dotted_name" {
                    (Some(import), Some(import))
                } else {
                    (None, None)
                };
                (local.and_then(|node| node_text(node, file.source)) == Some(name))
                    .then_some((imported, local))
            })
            .collect();
        let [(Some(imported), Some(local))] = imports.as_slice() else {
            return false;
        };
        if !matches!(
            node_text(*imported, file.source),
            Some("Mock" | "MagicMock")
        ) {
            return false;
        }
        let mut stack = vec![file.root];
        while let Some(node) = stack.pop() {
            if node.kind() == "identifier" {
                let text = node_text(node, file.source);
                if matches!(
                    text,
                    Some("exec" | "eval" | "globals" | "locals" | "vars" | "__import__")
                ) {
                    return false;
                }
                if text == Some(name)
                    && !(local.start_byte() <= node.start_byte()
                        && node.end_byte() <= local.end_byte())
                    && !node.parent().is_some_and(|call| {
                        call.kind() == "call"
                            && call.child_by_field_name("function").map(|n| n.id())
                                == Some(node.id())
                    })
                {
                    return false;
                }
            }
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
        }
        true
    })
}

fn shadows_unittest(directory: &std::path::Path) -> bool {
    match directory.try_exists() {
        Ok(false) => return false,
        Ok(true) => {}
        Err(_) => return true,
    }
    let Ok(mut entries) = std::fs::read_dir(directory) else {
        return true;
    };
    entries.any(|entry| {
        let Ok(entry) = entry else {
            return true;
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return true;
        };
        name == "unittest"
            || (name.starts_with("unittest.")
                && matches!(
                    std::path::Path::new(name)
                        .extension()
                        .and_then(|extension| extension.to_str()),
                    Some("py" | "pyi" | "pyc" | "pyd" | "so")
                ))
    })
}
