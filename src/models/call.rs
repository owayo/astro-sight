use super::location::Range;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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

/// The call graph response for a single file (full mode).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallGraph {
    pub language: String,
    pub calls: Vec<CallEdge>,
}

// ── Compact (token-optimized) variants ──

/// A callee in compact format.
#[derive(Debug, Clone, Serialize)]
pub struct CompactCallee {
    pub name: String,
    pub ln: usize,
    pub col: usize,
}

/// A group of calls from the same caller.
#[derive(Debug, Clone, Serialize)]
pub struct CompactCallGroup {
    pub caller: String,
    pub range: [usize; 4],
    pub callees: Vec<CompactCallee>,
}

/// Token-optimized call graph: calls grouped by caller.
#[derive(Debug, Clone, Serialize)]
pub struct CompactCallGraph {
    pub lang: String,
    pub calls: Vec<CompactCallGroup>,
}

impl CallGraph {
    pub fn to_compact(&self) -> CompactCallGraph {
        // Group by caller name, preserving first-seen order
        let mut order: Vec<String> = Vec::new();
        let mut groups: HashMap<String, CompactCallGroup> = HashMap::new();

        for edge in &self.calls {
            let key = edge.caller.name.clone();
            let group = groups.entry(key.clone()).or_insert_with(|| {
                order.push(key);
                CompactCallGroup {
                    caller: edge.caller.name.clone(),
                    range: [
                        edge.caller.range.start.line,
                        edge.caller.range.start.column,
                        edge.caller.range.end.line,
                        edge.caller.range.end.column,
                    ],
                    callees: Vec::new(),
                }
            });
            group.callees.push(CompactCallee {
                name: edge.callee.name.clone(),
                ln: edge.call_site.line,
                col: edge.call_site.column,
            });
        }

        CompactCallGraph {
            lang: self.language.clone(),
            calls: order
                .into_iter()
                .filter_map(|k| groups.remove(&k))
                .collect(),
        }
    }
}
