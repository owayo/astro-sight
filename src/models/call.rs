use super::location::Range;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 呼び出し位置（行と列は 0 始まり）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallSite {
    pub line: usize,
    pub column: usize,
}

/// 呼び出し元または呼び出し先の記述。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallEndpoint {
    pub name: String,
    pub range: Range,
}

/// 特定の呼び出し位置における単一の呼び出しエッジ（caller → callee）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallEdge {
    pub caller: CallEndpoint,
    pub callee: CallEndpoint,
    pub call_site: CallSite,
}

/// 単一ファイルのコールグラフレスポンス（full モード）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallGraph {
    pub language: String,
    pub calls: Vec<CallEdge>,
}

// ── compact（トークン最適化）形式 ──

/// compact 形式の呼び出し先。
#[derive(Debug, Clone, Serialize)]
pub struct CompactCallee {
    pub name: String,
    pub ln: usize,
    pub col: usize,
}

/// 同じ呼び出し元からの呼び出しグループ。
#[derive(Debug, Clone, Serialize)]
pub struct CompactCallGroup {
    pub caller: String,
    pub range: [usize; 4],
    pub callees: Vec<CompactCallee>,
}

/// 呼び出し元ごとにグループ化したトークン最適化コールグラフ。
#[derive(Debug, Clone, Serialize)]
pub struct CompactCallGraph {
    pub lang: String,
    pub calls: Vec<CompactCallGroup>,
}

impl CallGraph {
    pub fn to_compact(&self) -> CompactCallGraph {
        // 初出順を維持しながら呼び出し元名でグループ化する
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
