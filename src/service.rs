use anyhow::{Result, bail};
use camino::Utf8Path;
use std::path::PathBuf;

use crate::cache::store::CacheStore;
use crate::engine::{calls, extractor, impact, parser, refs, snippet, symbols};
use crate::error::{AstroError, ErrorCode};
use crate::models::call::CallGraph;
use crate::models::impact::ContextResult;
use crate::models::location::LocationKey;
use crate::models::reference::RefsResult;
use crate::models::response::AstgenResponse;

// ---------------------------------------------------------------------------
// AppService: unified core logic for CLI / Session / MCP
// ---------------------------------------------------------------------------

pub struct AppService {
    workspace_root: Option<PathBuf>,
    max_input_size: usize,
}

/// Parameters for AST extraction.
pub struct AstParams<'a> {
    pub path: &'a str,
    pub line: Option<usize>,
    pub col: Option<usize>,
    pub end_line: Option<usize>,
    pub end_col: Option<usize>,
    pub depth: usize,
    pub context_lines: usize,
}

impl Default for AppService {
    fn default() -> Self {
        Self::new()
    }
}

impl AppService {
    /// Create an unrestricted service (CLI mode).
    pub fn new() -> Self {
        Self {
            workspace_root: None,
            max_input_size: 0,
        }
    }

    /// Create a sandboxed service (MCP mode) that restricts paths to `root`.
    /// The root is canonicalized and must be a valid, non-empty directory.
    pub fn sandboxed(root: PathBuf) -> Result<Self> {
        let canonical_root = std::fs::canonicalize(&root).map_err(|_| {
            AstroError::new(
                ErrorCode::InvalidRequest,
                format!("Invalid workspace root: {}", root.display()),
            )
        })?;
        if canonical_root.as_os_str().is_empty() {
            bail!(AstroError::new(
                ErrorCode::InvalidRequest,
                "Workspace root must not be empty",
            ));
        }
        Ok(Self {
            workspace_root: Some(canonical_root),
            max_input_size: 100 * 1024 * 1024, // 100 MB
        })
    }

    /// Create a sandboxed service from an optional workspace spec (Session mode).
    pub fn from_env() -> Self {
        match std::env::var("ASTRO_SIGHT_WORKSPACE") {
            Ok(ws) if !ws.is_empty() => {
                Self::sandboxed(PathBuf::from(ws)).unwrap_or_else(|_| Self::new())
            }
            _ => Self::new(),
        }
    }

    // -----------------------------------------------------------------------
    // Validation helpers
    // -----------------------------------------------------------------------

    /// Validate and canonicalize a file path. Returns the canonical path.
    fn validate_path(&self, path: &str) -> Result<PathBuf> {
        let canonical = std::fs::canonicalize(path).map_err(|_| {
            AstroError::new(ErrorCode::FileNotFound, format!("File not found: {path}"))
        })?;
        if let Some(root) = &self.workspace_root
            && !canonical.starts_with(root)
        {
            bail!(AstroError::new(
                ErrorCode::PathOutOfBounds,
                format!("Path outside workspace boundary: {path}"),
            ));
        }
        Ok(canonical)
    }

    /// Validate and canonicalize a directory path. Returns the canonical path.
    fn validate_dir(&self, dir: &str) -> Result<PathBuf> {
        let canonical = std::fs::canonicalize(dir).map_err(|_| {
            AstroError::new(
                ErrorCode::FileNotFound,
                format!("Directory not found: {dir}"),
            )
        })?;
        if let Some(root) = &self.workspace_root
            && !canonical.starts_with(root)
        {
            bail!(AstroError::new(
                ErrorCode::PathOutOfBounds,
                format!("Directory outside workspace boundary: {dir}"),
            ));
        }
        Ok(canonical)
    }

    fn validate_input_size(&self, data: &str) -> Result<()> {
        if self.max_input_size > 0 && data.len() > self.max_input_size {
            bail!(AstroError::new(
                ErrorCode::InvalidRequest,
                format!(
                    "Input exceeds maximum size ({} bytes > {} bytes)",
                    data.len(),
                    self.max_input_size
                ),
            ));
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Core operations
    // -----------------------------------------------------------------------

    /// Extract AST at a given position/range with optional snippet + diagnostics.
    pub fn extract_ast(&self, p: &AstParams<'_>) -> Result<AstgenResponse> {
        let canonical = self.validate_path(p.path)?;
        let utf8_path = Utf8Path::new(canonical.to_str().unwrap_or(p.path));

        let source = parser::read_file(utf8_path)?;
        let (tree, lang_id) = parser::parse_file(utf8_path, &source)?;
        let root = tree.root_node();

        // Use original path in the response location for user readability
        let location = match (p.line, p.col, p.end_line, p.end_col) {
            (Some(l), Some(c), Some(el), Some(ec)) => LocationKey::range(p.path, l, c, el, ec),
            (Some(l), Some(c), _, _) => LocationKey::point(p.path, l, c),
            (Some(l), None, _, _) => LocationKey::point(p.path, l, 0),
            _ => LocationKey::file_only(p.path),
        };

        let ast_nodes = match (p.line, p.end_line) {
            (Some(l), Some(el)) => {
                let c = p.col.unwrap_or(0);
                let ec = p.end_col.unwrap_or(usize::MAX);
                extractor::extract_range(root, &source, l, c, el, ec, p.depth)
            }
            (Some(l), None) => {
                let c = p.col.unwrap_or(0);
                extractor::extract_at_point(root, &source, l, c, p.depth)
            }
            _ => extractor::extract_full(root, &source, p.depth),
        };

        let source_str = std::str::from_utf8(&source).unwrap_or("");
        let snip = match (p.line, p.end_line) {
            (Some(l), Some(el)) => Some(snippet::generate_range_snippet(
                source_str,
                l,
                el,
                p.context_lines,
            )),
            (Some(l), None) => Some(snippet::generate_snippet(source_str, l, p.context_lines)),
            _ => None,
        };

        let mut response = AstgenResponse::success(location, lang_id);
        response.hash = Some(CacheStore::hash(&source));
        response.ast = Some(ast_nodes);
        response.snippet = snip;
        collect_diagnostics(root, &mut response);
        Ok(response)
    }

    /// Extract symbols from a source file with diagnostics.
    pub fn extract_symbols(&self, path: &str) -> Result<AstgenResponse> {
        let canonical = self.validate_path(path)?;
        let utf8_path = Utf8Path::new(canonical.to_str().unwrap_or(path));

        let source = parser::read_file(utf8_path)?;
        let (tree, lang_id) = parser::parse_file(utf8_path, &source)?;
        let root = tree.root_node();

        let syms = symbols::extract_symbols(root, &source, lang_id)?;

        let location = LocationKey::file_only(path);
        let mut response = AstgenResponse::success(location, lang_id);
        response.hash = Some(CacheStore::hash(&source));
        response.symbols = Some(syms);
        collect_diagnostics(root, &mut response);
        Ok(response)
    }

    /// Extract call graph from a source file.
    pub fn extract_calls(&self, path: &str, function: Option<&str>) -> Result<CallGraph> {
        let canonical = self.validate_path(path)?;
        let utf8_path = Utf8Path::new(canonical.to_str().unwrap_or(path));

        let source = parser::read_file(utf8_path)?;
        let (tree, lang_id) = parser::parse_file(utf8_path, &source)?;
        let root = tree.root_node();

        let edges = calls::extract_calls(root, &source, lang_id, function)?;

        Ok(CallGraph {
            version: env!("CARGO_PKG_VERSION").to_string(),
            language: format!("{lang_id:?}").to_lowercase(),
            calls: edges,
        })
    }

    /// Search for symbol references across files.
    pub fn find_references(&self, name: &str, dir: &str, glob: Option<&str>) -> Result<RefsResult> {
        let canonical_dir = self.validate_dir(dir)?;

        let references = refs::find_references(name, &canonical_dir, glob)?;

        Ok(RefsResult {
            version: env!("CARGO_PKG_VERSION").to_string(),
            symbol: name.to_string(),
            references,
        })
    }

    /// Analyze the impact of a unified diff on the codebase.
    pub fn analyze_context(&self, diff: &str, dir: &str) -> Result<ContextResult> {
        let canonical_dir = self.validate_dir(dir)?;
        self.validate_input_size(diff)?;

        impact::analyze_impact(diff, &canonical_dir)
    }
}

// ---------------------------------------------------------------------------
// Diagnostics helper (shared by all code paths via AppService)
// ---------------------------------------------------------------------------

fn collect_diagnostics(root: tree_sitter::Node<'_>, response: &mut AstgenResponse) {
    if root.has_error() {
        collect_error_nodes(root, &mut response.diagnostics);
    }
}

fn collect_error_nodes(
    node: tree_sitter::Node<'_>,
    diagnostics: &mut Vec<crate::models::diagnostic::Diagnostic>,
) {
    if node.is_error() || node.is_missing() {
        diagnostics.push(crate::models::diagnostic::Diagnostic {
            severity: crate::models::diagnostic::Severity::Error,
            message: format!(
                "Parse error: {} at {}:{}",
                node.kind(),
                node.start_position().row,
                node.start_position().column
            ),
            line: Some(node.start_position().row),
            column: Some(node.start_position().column),
        });
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.has_error() || child.is_error() || child.is_missing() {
            collect_error_nodes(child, diagnostics);
        }
    }
}
