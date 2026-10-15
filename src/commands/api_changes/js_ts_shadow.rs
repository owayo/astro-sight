//! JS/TS の共有字句スコープ解決を API 差分でも使う。

pub(crate) use crate::engine::js_scope::{
    LocalResolution, Namespace, ScopeBinding, import_binds_name, pattern_binds_name,
    resolve_reference_binding, resolve_scope_binding, visit_import_bindings,
};
