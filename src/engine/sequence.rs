use std::collections::HashSet;

use crate::models::call::CallEdge;
use crate::models::sequence::SequenceDiagramResult;

/// Generate a Mermaid sequence diagram from call edges.
pub fn generate_sequence_diagram(edges: &[CallEdge], language: &str) -> SequenceDiagramResult {
    // 1. Sort edges by (line, column) for stable execution order
    let mut sorted: Vec<&CallEdge> = edges.iter().collect();
    sorted.sort_by_key(|e| (e.call_site.line, e.call_site.column));

    // 2. Collect participants in first-appearance order with O(1) dedup
    let mut participants: Vec<String> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for edge in &sorted {
        if seen.insert(&edge.caller.name) {
            participants.push(edge.caller.name.clone());
        }
        if seen.insert(&edge.callee.name) {
            participants.push(edge.callee.name.clone());
        }
    }

    // 3. Generate Mermaid text with sanitized names
    let mut lines: Vec<String> = Vec::new();
    lines.push("sequenceDiagram".to_string());
    for p in &participants {
        let safe = sanitize_mermaid(p);
        lines.push(format!("participant {safe}"));
    }
    for edge in &sorted {
        let caller = sanitize_mermaid(&edge.caller.name);
        let callee = sanitize_mermaid(&edge.callee.name);
        lines.push(format!("{caller}->>{callee}: {callee}()"));
    }

    let diagram = lines.join("\n");

    SequenceDiagramResult {
        language: language.to_string(),
        participants,
        diagram,
    }
}

/// Sanitize a name for use in Mermaid diagrams.
/// Replaces characters that would break Mermaid syntax.
fn sanitize_mermaid(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            ';' | '#' | ':' | '>' | '<' | '{' | '}' | '|' | '`' | '"' | '\'' | '\n' | '\r' => '_',
            _ => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::call::{CallEndpoint, CallSite};
    use crate::models::location::{Point, Range};

    fn make_edge(caller: &str, callee: &str, line: usize) -> CallEdge {
        let zero_range = Range {
            start: Point { line: 0, column: 0 },
            end: Point { line: 0, column: 0 },
        };
        CallEdge {
            caller: CallEndpoint {
                name: caller.to_string(),
                range: zero_range,
            },
            callee: CallEndpoint {
                name: callee.to_string(),
                range: zero_range,
            },
            call_site: CallSite { line, column: 0 },
        }
    }

    #[test]
    fn empty_edges() {
        let result = generate_sequence_diagram(&[], "rust");
        assert_eq!(result.language, "rust");
        assert!(result.participants.is_empty());
        assert_eq!(result.diagram, "sequenceDiagram");
    }

    #[test]
    fn single_call() {
        let edges = vec![make_edge("main", "run", 5)];
        let result = generate_sequence_diagram(&edges, "rust");
        assert_eq!(result.participants, vec!["main", "run"]);
        assert!(result.diagram.contains("sequenceDiagram"));
        assert!(result.diagram.contains("participant main"));
        assert!(result.diagram.contains("participant run"));
        assert!(result.diagram.contains("main->>run: run()"));
    }

    #[test]
    fn self_call() {
        let edges = vec![make_edge("factorial", "factorial", 10)];
        let result = generate_sequence_diagram(&edges, "rust");
        assert_eq!(result.participants, vec!["factorial"]);
        assert!(
            result
                .diagram
                .contains("factorial->>factorial: factorial()")
        );
    }

    #[test]
    fn sorted_by_line() {
        let edges = vec![
            make_edge("main", "cleanup", 20),
            make_edge("main", "init", 5),
            make_edge("main", "run", 10),
        ];
        let result = generate_sequence_diagram(&edges, "rust");
        assert_eq!(result.participants, vec!["main", "init", "run", "cleanup"]);

        // Check ordering in diagram
        let init_pos = result.diagram.find("main->>init").unwrap();
        let run_pos = result.diagram.find("main->>run").unwrap();
        let cleanup_pos = result.diagram.find("main->>cleanup").unwrap();
        assert!(init_pos < run_pos);
        assert!(run_pos < cleanup_pos);
    }

    #[test]
    fn participant_dedup() {
        let edges = vec![
            make_edge("main", "foo", 5),
            make_edge("main", "bar", 10),
            make_edge("foo", "bar", 15),
        ];
        let result = generate_sequence_diagram(&edges, "rust");
        assert_eq!(result.participants, vec!["main", "foo", "bar"]);
    }

    fn make_edge_with_col(caller: &str, callee: &str, line: usize, column: usize) -> CallEdge {
        let zero_range = Range {
            start: Point { line: 0, column: 0 },
            end: Point { line: 0, column: 0 },
        };
        CallEdge {
            caller: CallEndpoint {
                name: caller.to_string(),
                range: zero_range,
            },
            callee: CallEndpoint {
                name: callee.to_string(),
                range: zero_range,
            },
            call_site: CallSite { line, column },
        }
    }

    #[test]
    fn same_line_sorted_by_column() {
        let edges = vec![
            make_edge_with_col("main", "bar", 5, 20),
            make_edge_with_col("main", "foo", 5, 5),
        ];
        let result = generate_sequence_diagram(&edges, "rust");
        // foo (col 5) should come before bar (col 20) in the diagram
        let foo_pos = result.diagram.find("main->>foo").unwrap();
        let bar_pos = result.diagram.find("main->>bar").unwrap();
        assert!(foo_pos < bar_pos);
    }

    #[test]
    fn special_chars_sanitized() {
        let edges = vec![make_edge("main", "foo<T>", 5)];
        let result = generate_sequence_diagram(&edges, "rust");
        // Original name in participants
        assert_eq!(result.participants, vec!["main", "foo<T>"]);
        // Sanitized in diagram
        assert!(result.diagram.contains("participant foo_T_"));
        assert!(result.diagram.contains("main->>foo_T_: foo_T_()"));
    }
}
