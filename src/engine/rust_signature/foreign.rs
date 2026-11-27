//! extern ブロックで宣言した関数の ABI。省略時の呼び出し規約は C。

use std::borrow::Cow;

use tree_sitter::Node;

pub(crate) struct Block<'tree, 'src> {
    pub(crate) modifier: Node<'tree>,
    pub(crate) body: Node<'tree>,
    pub(crate) abi: Cow<'src, str>,
    pub(crate) source: &'src [u8],
}

impl<'tree, 'src> Block<'tree, 'src> {
    pub(crate) fn read(item: Node<'tree>, source: &'src [u8]) -> Option<Self> {
        if item.kind() != "foreign_mod_item" {
            return None;
        }
        let body = item.child_by_field_name("body")?;
        let modifier = super::direct_named_child(item, "extern_modifier")?;
        let mut cursor = modifier.walk();
        let literal = modifier.named_children(&mut cursor).find(|n| !n.is_extra());
        let abi = match literal {
            None => Cow::Borrowed("C"),
            Some(node) if node.kind() == "string_literal" => {
                let text = node
                    .utf8_text(source)
                    .ok()?
                    .strip_prefix('"')?
                    .strip_suffix('"')?;
                if text.contains('\\') {
                    Cow::Owned(
                        String::from_utf8(crate::engine::refs::decode_rust_string_escapes(
                            text.as_bytes(),
                        ))
                        .ok()?,
                    )
                } else {
                    Cow::Borrowed(text)
                }
            }
            Some(node) if node.kind() == "raw_string_literal" => Cow::Borrowed(
                super::direct_named_child(node, "string_content")?
                    .utf8_text(source)
                    .ok()?,
            ),
            _ => return None,
        };
        Some(Self {
            modifier,
            body,
            abi,
            source,
        })
    }

    pub(crate) fn for_function(function: Node<'tree>, source: &'src [u8]) -> Option<Self> {
        if function.kind() != "function_signature_item" {
            return None;
        }
        let body = function.parent()?;
        let block = Self::read(body.parent()?, source)?;
        (body == block.body).then_some(block)
    }

    pub(crate) fn signature(&self, function: Node<'_>) -> Option<String> {
        let declaration = super::normalize_rust_parameter_binding_signature(
            function,
            self.source,
            function.start_byte(),
            function.end_byte(),
        )
        .or_else(|| function.utf8_text(self.source).ok().map(str::to_owned))?;
        Some(format!(
            "unsafe extern \"{}\" {{ {declaration} }}",
            self.abi
        ))
    }
}
