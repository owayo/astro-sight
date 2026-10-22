use std::collections::HashSet;

use serde::{Deserialize, Deserializer, de::Error};

use crate::{engine::parser, language::LangId};

/// dead-code が外部から呼ばれる Swift protocol メソッドとして扱う追加要件。
/// デシリアライズ時に検証し、未検証の行を外から変更できないようにする。
#[derive(Debug, Clone, Default)]
pub struct SwiftProtocolMethods(Vec<SwiftProtocolMethod>);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct SwiftProtocolMethod {
    pub(crate) protocol: String,
    pub(crate) method: String,
    pub(crate) labels: Vec<String>,
    pub(crate) parameter_types: Vec<String>,
    pub(crate) return_type: String,
    pub(crate) is_static: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMethod {
    protocol: String,
    #[serde(alias = "name")]
    method: String,
    labels: Vec<String>,
    parameter_types: Vec<String>,
    return_type: String,
    #[serde(default, rename = "static")]
    is_static: bool,
}

impl<'de> Deserialize<'de> for SwiftProtocolMethods {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let rows = Vec::<RawMethod>::deserialize(deserializer)?;
        let mut result = Vec::with_capacity(rows.len());
        let mut seen = HashSet::new();
        for (index, row) in rows.into_iter().enumerate() {
            let fail = |reason: &str| {
                D::Error::custom(format!("swift_protocol_methods[{index}]: {reason}"))
            };
            if row.labels.len() != row.parameter_types.len() {
                return Err(fail("labels and parameter_types must have the same length"));
            }
            let protocol = row.protocol.trim().to_owned();
            if protocol.split('.').count() < 2 || !protocol.split('.').all(valid_identifier) {
                return Err(fail("protocol must be a qualified Swift name"));
            }
            if !valid_identifier(&row.method)
                || !row.labels.iter().all(|label| valid_identifier(label))
            {
                return Err(fail("method and labels must be Swift identifiers"));
            }
            let parameter_types = row
                .parameter_types
                .iter()
                .map(|value| validated_type(value, false))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| fail("invalid parameter_types"))?;
            let return_type = validated_type(&row.return_type, true)
                .ok_or_else(|| fail("invalid return_type"))?;
            let method = SwiftProtocolMethod {
                protocol,
                method: row.method,
                labels: row.labels,
                parameter_types,
                return_type,
                is_static: row.is_static,
            };
            if !seen.insert(method.clone()) {
                return Err(fail("duplicate method requirement"));
            }
            result.push(method);
        }
        Ok(Self(result))
    }
}

impl SwiftProtocolMethods {
    pub(crate) fn iter(&self) -> impl Iterator<Item = &SwiftProtocolMethod> {
        self.0.iter()
    }
}

fn valid_identifier(value: &str) -> bool {
    if value.is_empty() || value.trim() != value {
        return false;
    }
    let source = format!("func {value}() {{}}");
    let Ok(tree) = parser::parse_source(source.as_bytes(), LangId::Swift) else {
        return false;
    };
    let root = tree.root_node();
    !root.has_error()
        && root.named_child_count() == 1
        && root.named_child(0).is_some_and(|function| {
            function.kind() == "function_declaration"
                && function.child_by_field_name("name").is_some_and(|name| {
                    name.kind() == "simple_identifier"
                        && name.utf8_text(source.as_bytes()).ok() == Some(value)
                })
        })
}

fn validated_type(value: &str, returns: bool) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let prefix = if returns {
        "func __witness() -> "
    } else {
        "func __witness(_ value: "
    };
    let suffix = if returns { " {}" } else { ") {}" };
    let source = format!("{prefix}{value}{suffix}");
    let tree = parser::parse_source(source.as_bytes(), LangId::Swift).ok()?;
    let root = tree.root_node();
    if root.has_error() || root.named_child_count() != 1 {
        return None;
    }
    let function = root.named_child(0)?;
    if function.kind() != "function_declaration" {
        return None;
    }
    let mut cursor = function.walk();
    let parameters: Vec<_> = function
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "parameter")
        .collect();
    if returns {
        if !parameters.is_empty() {
            return None;
        }
        let node = function.child_by_field_name("return_type")?;
        if node.start_byte() != prefix.len() || node.end_byte() != prefix.len() + value.len() {
            return None;
        }
    } else if parameters.len() != 1
        || parameters[0].end_byte() != prefix.len() + value.len()
        || function.child_by_field_name("default_value").is_some()
    {
        return None;
    }
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if node.is_missing()
            || matches!(
                node.kind(),
                "comment" | "multiline_comment" | "directive" | "attribute"
            )
        {
            return None;
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    Some(compact_type(value))
}

pub(crate) fn compact_type(value: &str) -> String {
    let value = value.trim();
    for modifier in ["inout", "some", "any"] {
        if let Some(rest) = value.strip_prefix(modifier)
            && rest.starts_with(char::is_whitespace)
        {
            return format!("{modifier} {}", compact_type(rest));
        }
    }
    value
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(extra: &str) -> String {
        format!(
            "[[swift_protocol_methods]]\nprotocol='Acme.P'\nmethod='refresh'\nlabels=['_']\nparameter_types=['Swift.String']\nreturn_type='Swift.Void'\n{extra}"
        )
    }

    #[test]
    fn swift_protocol_configuration_validates_direct_deserialization() {
        let config: crate::config::Config = toml::from_str(&row("static=true")).unwrap();
        let method = config.swift_protocol_methods.iter().next().unwrap();
        assert!(method.is_static);
        for invalid in [
            row("typo=true"),
            row("").replace("['Swift.String']", "[]"),
            row("").replace("'Acme.P'", "''"),
            row("").replace("'refresh'", "'bad-name'"),
            row("").replace("'Swift.String'", "'Int, other: Int'"),
            row("").replace("'Swift.Void'", "'Int {} func other() -> Int'"),
            format!("{}\n{}", row(""), row("")),
            row("").replace("parameter_types=['Swift.String']\n", ""),
        ] {
            assert!(
                toml::from_str::<crate::config::Config>(&invalid).is_err(),
                "{invalid}"
            );
        }
        assert!(validated_type("inout Swift.Hasher", false).is_some());
        assert!(validated_type("()", true).is_some());
    }
}
