use anyhow::{Result, bail};
use camino::Utf8Path;
use std::path::PathBuf;
use tracing::{debug, warn};

use crate::cache::store::CacheStore;
use crate::engine::{calls, extractor, impact, imports, lint, parser, refs, snippet, symbols};
use crate::error::{AstroError, ErrorCode};
use crate::models::call::CallGraph;
use crate::models::cochange::CoChangeResult;
use crate::models::impact::ContextResult;
use crate::models::import::ImportsResult;
use crate::models::location::LocationKey;
use crate::models::reference::RefsResult;
use crate::models::response::AstgenResponse;
use crate::models::sequence::SequenceDiagramResult;

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
            warn!(path = path, "validate_path: file not found");
            AstroError::new(ErrorCode::FileNotFound, format!("File not found: {path}"))
        })?;
        if let Some(root) = &self.workspace_root
            && !canonical.starts_with(root)
        {
            warn!(
                path = path,
                "validate_path: path outside workspace boundary"
            );
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
        debug!(
            path = p.path,
            line = ?p.line,
            col = ?p.col,
            end_line = ?p.end_line,
            end_col = ?p.end_col,
            depth = p.depth,
            "extract_ast called"
        );
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
        debug!(
            path = p.path,
            language = ?lang_id,
            ast_nodes = response.ast.as_ref().map(|a| a.len()).unwrap_or(0),
            diagnostics = response.diagnostics.len(),
            "extract_ast completed"
        );
        Ok(response)
    }

    /// Extract symbols from a source file with diagnostics.
    pub fn extract_symbols(&self, path: &str) -> Result<AstgenResponse> {
        debug!(path = path, "extract_symbols called");
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
        debug!(
            path = path,
            language = ?lang_id,
            symbols = response.symbols.as_ref().map(|s| s.len()).unwrap_or(0),
            diagnostics = response.diagnostics.len(),
            "extract_symbols completed"
        );
        Ok(response)
    }

    /// Extract call graph from a source file.
    pub fn extract_calls(&self, path: &str, function: Option<&str>) -> Result<CallGraph> {
        debug!(path = path, function = ?function, "extract_calls called");
        let canonical = self.validate_path(path)?;
        let utf8_path = Utf8Path::new(canonical.to_str().unwrap_or(path));

        let source = parser::read_file(utf8_path)?;
        let (tree, lang_id) = parser::parse_file(utf8_path, &source)?;
        let root = tree.root_node();

        let edges = calls::extract_calls(root, &source, lang_id, function)?;

        let graph = CallGraph {
            language: format!("{lang_id:?}").to_lowercase(),
            calls: edges,
        };
        debug!(
            path = path,
            function = ?function,
            call_edges = graph.calls.len(),
            "extract_calls completed"
        );
        Ok(graph)
    }

    /// Generate a Mermaid sequence diagram from a source file's call graph.
    pub fn generate_sequence(
        &self,
        path: &str,
        function: Option<&str>,
    ) -> Result<SequenceDiagramResult> {
        debug!(path = path, function = ?function, "generate_sequence called");
        let canonical = self.validate_path(path)?;
        let utf8_path = Utf8Path::new(canonical.to_str().unwrap_or(path));

        let source = parser::read_file(utf8_path)?;
        let (tree, lang_id) = parser::parse_file(utf8_path, &source)?;
        let root = tree.root_node();

        let edges = calls::extract_calls(root, &source, lang_id, function)?;
        let language = format!("{lang_id:?}").to_lowercase();

        let result = crate::engine::sequence::generate_sequence_diagram(&edges, &language);
        debug!(
            path = path,
            participants = result.participants.len(),
            "generate_sequence completed"
        );
        Ok(result)
    }

    /// Extract import/export dependencies from a source file.
    pub fn extract_imports(&self, path: &str) -> Result<ImportsResult> {
        debug!(path = path, "extract_imports called");
        let canonical = self.validate_path(path)?;
        let utf8_path = Utf8Path::new(canonical.to_str().unwrap_or(path));

        let source = parser::read_file(utf8_path)?;
        let (tree, lang_id) = parser::parse_file(utf8_path, &source)?;
        let root = tree.root_node();

        let edges = imports::extract_imports(root, &source, lang_id)?;
        let language = format!("{lang_id:?}").to_lowercase();

        let result = ImportsResult {
            language,
            imports: edges,
        };
        debug!(
            path = path,
            imports = result.imports.len(),
            "extract_imports completed"
        );
        Ok(result)
    }

    /// Lint a source file against the given rules.
    pub fn lint_file(
        &self,
        path: &str,
        rules: &[crate::models::lint::Rule],
    ) -> Result<crate::models::lint::LintResult> {
        debug!(path = path, rules = rules.len(), "lint_file called");
        let canonical = self.validate_path(path)?;
        let utf8_path = Utf8Path::new(canonical.to_str().unwrap_or(path));

        let source = parser::read_file(utf8_path)?;
        let (tree, lang_id) = parser::parse_file(utf8_path, &source)?;
        let root = tree.root_node();

        let (matches, warnings) = lint::lint_file(root, &source, lang_id, rules)?;
        let language = lang_id.to_string();

        let result = crate::models::lint::LintResult {
            language,
            matches,
            warnings,
        };
        debug!(
            path = path,
            matches = result.matches.len(),
            "lint_file completed"
        );
        Ok(result)
    }

    /// Search for symbol references across files.
    pub fn find_references(&self, name: &str, dir: &str, glob: Option<&str>) -> Result<RefsResult> {
        debug!(name = name, dir = dir, glob = ?glob, "find_references called");
        let canonical_dir = self.validate_dir(dir)?;

        let references = refs::find_references(name, &canonical_dir, glob)?;

        // Convert absolute paths to relative (relative to dir)
        let references = relativize_paths(references, &canonical_dir);

        let result = RefsResult {
            symbol: name.to_string(),
            references,
        };
        debug!(
            name = name,
            dir = dir,
            references = result.references.len(),
            "find_references completed"
        );
        Ok(result)
    }

    /// Analyze the impact of a unified diff on the codebase.
    pub fn analyze_context(&self, diff: &str, dir: &str) -> Result<ContextResult> {
        debug!(dir = dir, diff_bytes = diff.len(), "analyze_context called");
        let canonical_dir = self.validate_dir(dir)?;
        self.validate_input_size(diff)?;

        let mut result = impact::analyze_impact(diff, &canonical_dir)?;

        // Convert absolute paths in impacted_callers to relative
        for change in &mut result.changes {
            for caller in &mut change.impacted_callers {
                if let Ok(rel) = std::path::Path::new(&caller.path).strip_prefix(&canonical_dir) {
                    caller.path = rel.to_string_lossy().to_string();
                }
            }
        }

        debug!(
            dir = dir,
            changes = result.changes.len(),
            total_affected = result
                .changes
                .iter()
                .map(|c| c.affected_symbols.len())
                .sum::<usize>(),
            total_callers = result
                .changes
                .iter()
                .map(|c| c.impacted_callers.len())
                .sum::<usize>(),
            "analyze_context completed"
        );
        Ok(result)
    }

    /// Analyze co-change patterns from git history.
    pub fn analyze_cochange(
        &self,
        dir: &str,
        lookback: usize,
        min_confidence: f64,
        filter_file: Option<&str>,
    ) -> Result<CoChangeResult> {
        debug!(
            dir = dir,
            lookback = lookback,
            min_confidence = min_confidence,
            filter_file = ?filter_file,
            "analyze_cochange called"
        );
        // Validate parameters
        const MAX_LOOKBACK: usize = 10_000;
        if lookback == 0 || lookback > MAX_LOOKBACK {
            bail!(AstroError::new(
                ErrorCode::InvalidRequest,
                format!("lookback must be 1..={MAX_LOOKBACK}, got {lookback}"),
            ));
        }
        if !min_confidence.is_finite() || !(0.0..=1.0).contains(&min_confidence) {
            bail!(AstroError::new(
                ErrorCode::InvalidRequest,
                format!(
                    "min_confidence must be a finite value in [0.0, 1.0], got {min_confidence}"
                ),
            ));
        }

        let canonical_dir = self.validate_dir(dir)?;
        let dir_str = canonical_dir.to_string_lossy();

        let result = crate::engine::cochange::analyze_cochange(
            &dir_str,
            lookback,
            min_confidence,
            filter_file,
        )?;
        debug!(
            dir = dir,
            entries = result.entries.len(),
            commits_analyzed = result.commits_analyzed,
            "analyze_cochange completed"
        );
        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// Convert absolute paths to relative (relative to `dir`).
/// Paths outside `dir` are left as-is.
fn relativize_paths(
    mut refs: Vec<crate::models::reference::SymbolReference>,
    dir: &std::path::Path,
) -> Vec<crate::models::reference::SymbolReference> {
    for r in &mut refs {
        if let Ok(rel) = std::path::Path::new(&r.path).strip_prefix(dir) {
            r.path = rel.to_string_lossy().to_string();
        }
    }
    refs
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
