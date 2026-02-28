use super::location::Range;
use serde::{Deserialize, Serialize};

/// A call site location (line and column, 0-indexed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallSite {
    pub line: usize,
    pub column: usize,
}

/// A caller or callee descriptor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallEndpoint {
    pub name: String,
    pub range: Range,
}

/// A single call edge: caller → callee at a specific call site.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallEdge {
    pub caller: CallEndpoint,
    pub callee: CallEndpoint,
    pub call_site: CallSite,
}

/// The call graph response for a single file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallGraph {
    pub version: String,
    pub language: String,
    pub calls: Vec<CallEdge>,
}
