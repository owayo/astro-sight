use serde::{Deserialize, Serialize};

/// A point in a source file (0-indexed line and column).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Point {
    pub line: usize,
    pub column: usize,
}

impl From<tree_sitter::Point> for Point {
    fn from(p: tree_sitter::Point) -> Self {
        Self {
            line: p.row,
            column: p.column,
        }
    }
}

/// A range in a source file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    pub start: Point,
    pub end: Point,
}

impl From<tree_sitter::Range> for Range {
    fn from(r: tree_sitter::Range) -> Self {
        Self {
            start: r.start_point.into(),
            end: r.end_point.into(),
        }
    }
}

/// A location key identifying a position in a file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocationKey {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_column: Option<usize>,
}

impl LocationKey {
    pub fn point(path: impl Into<String>, line: usize, column: usize) -> Self {
        Self {
            path: path.into(),
            line: Some(line),
            column: Some(column),
            end_line: None,
            end_column: None,
        }
    }

    pub fn range(
        path: impl Into<String>,
        start_line: usize,
        start_col: usize,
        end_line: usize,
        end_col: usize,
    ) -> Self {
        Self {
            path: path.into(),
            line: Some(start_line),
            column: Some(start_col),
            end_line: Some(end_line),
            end_column: Some(end_col),
        }
    }

    pub fn file_only(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            line: None,
            column: None,
            end_line: None,
            end_column: None,
        }
    }
}
