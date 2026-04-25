use anyhow::{Result, bail};
use camino::Utf8Path;
use std::path::PathBuf;
use tracing::{debug, warn};

use crate::cache::store::CacheStore;
use crate::engine::{calls, extractor, impact, imports, lint, parser, refs, snippet, symbols};
use crate::error::{AstroError, ErrorCode};
use crate::models::call::CallGraph;
use crate::models::cochange::{CoChangeOptions, CoChangeResult};
use crate::models::impact::ContextResult;
use crate::models::import::ImportsResult;
use crate::models::location::LocationKey;
use crate::models::reference::RefsResult;
use crate::models::response::AstgenResponse;
use crate::models::sequence::SequenceDiagramResult;

// ---------------------------------------------------------------------------
// AppService: CLI / Session / MCP で共有する中核ロジック
// ---------------------------------------------------------------------------

pub struct AppService {
    workspace_root: Option<PathBuf>,
    max_input_size: usize,
}

/// AST 抽出パラメータ。
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
    /// 制限なしのサービスを生成する（CLI モード）。
    pub fn new() -> Self {
        Self {
            workspace_root: None,
            max_input_size: 0,
        }
    }

    /// パスを `root` 配下に制限したサービスを生成する（MCP モード）。
    /// `root` は正規化可能な空でないディレクトリでなければならない。
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
        if !canonical_root.is_dir() {
            bail!(AstroError::new(
                ErrorCode::InvalidRequest,
                format!("Workspace root must be a directory: {}", root.display()),
            ));
        }
        Ok(Self {
            workspace_root: Some(canonical_root),
            max_input_size: 100 * 1024 * 1024, // 100 MB
        })
    }

    /// 環境変数からサービスを生成する（Session モード）。
    /// `ASTRO_SIGHT_WORKSPACE` が指定されている場合は、
    /// 不正な値でも無制限モードへフォールバックしない。
    pub fn from_env() -> Result<Self> {
        match std::env::var("ASTRO_SIGHT_WORKSPACE") {
            Ok(ws) => {
                if ws.is_empty() {
                    bail!(AstroError::new(
                        ErrorCode::InvalidRequest,
                        "Invalid ASTRO_SIGHT_WORKSPACE: value must not be empty",
                    ));
                }
                Self::sandboxed(PathBuf::from(&ws)).map_err(|e| {
                    if let Some(ae) = e.downcast_ref::<AstroError>() {
                        AstroError::new(
                            ae.code,
                            format!("Invalid ASTRO_SIGHT_WORKSPACE ({ws}): {}", ae.message),
                        )
                        .into()
                    } else {
                        AstroError::new(
                            ErrorCode::InvalidRequest,
                            format!("Invalid ASTRO_SIGHT_WORKSPACE ({ws}): {e}"),
                        )
                        .into()
                    }
                })
            }
            Err(std::env::VarError::NotPresent) => Ok(Self::new()),
            Err(std::env::VarError::NotUnicode(_)) => bail!(AstroError::new(
                ErrorCode::InvalidRequest,
                "Invalid ASTRO_SIGHT_WORKSPACE: value is not valid UTF-8",
            )),
        }
    }

    // -----------------------------------------------------------------------
    // 検証ヘルパー
    // -----------------------------------------------------------------------

    /// ファイルパスを検証して正規化し、正規化済みパスを返す。
    fn validate_path(&self, path: &str) -> Result<PathBuf> {
        let canonical = std::fs::canonicalize(path).map_err(|_| {
            warn!(path = path, "⚠️ validate_path: file not found");
            AstroError::new(ErrorCode::FileNotFound, format!("File not found: {path}"))
        })?;
        if let Some(root) = &self.workspace_root
            && !canonical.starts_with(root)
        {
            warn!(
                path = path,
                "🚫 validate_path: path outside workspace boundary"
            );
            bail!(AstroError::new(
                ErrorCode::PathOutOfBounds,
                format!("Path outside workspace boundary: {path}"),
            ));
        }
        Ok(canonical)
    }

    /// ディレクトリパスを検証して正規化し、正規化済みパスを返す。
    fn validate_dir(&self, dir: &str) -> Result<PathBuf> {
        let canonical = std::fs::canonicalize(dir).map_err(|_| {
            AstroError::new(
                ErrorCode::FileNotFound,
                format!("Directory not found: {dir}"),
            )
        })?;
        if !canonical.is_dir() {
            bail!(AstroError::new(
                ErrorCode::InvalidRequest,
                format!("Path is not a directory: {dir}"),
            ));
        }
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
    // コア操作
    // -----------------------------------------------------------------------

    /// 指定位置または範囲の AST を抽出し、必要に応じてスニペットと診断情報を付与する。
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

        // 利用者が見やすいよう、レスポンスの location には元のパス表記を残す。
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
            // AST 抽出時の text/snippet は長大行を内部で切り詰め、minified/生成コードでも
            // JSON 応答サイズが跳ね上がらないようにする。
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

    /// ソースファイルからシンボルを抽出し、診断情報も返す。
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

    /// ソースファイルからコールグラフを抽出する。
    pub fn extract_calls(&self, path: &str, function: Option<&str>) -> Result<CallGraph> {
        debug!(path = path, function = ?function, "extract_calls called");
        let canonical = self.validate_path(path)?;
        let utf8_path = Utf8Path::new(canonical.to_str().unwrap_or(path));

        let source = parser::read_file(utf8_path)?;
        let (tree, lang_id) = parser::parse_file(utf8_path, &source)?;
        let root = tree.root_node();

        let edges = calls::extract_calls(root, &source, lang_id, function)?;

        let graph = CallGraph {
            language: lang_id.to_string(),
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

    /// ソースファイルのコールグラフから Mermaid のシーケンス図を生成する。
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
        let language = lang_id.to_string();

        let result = crate::engine::sequence::generate_sequence_diagram(&edges, &language);
        debug!(
            path = path,
            participants = result.participants.len(),
            "generate_sequence completed"
        );
        Ok(result)
    }

    /// ソースファイルから import/export 依存関係を抽出する。
    pub fn extract_imports(&self, path: &str) -> Result<ImportsResult> {
        debug!(path = path, "extract_imports called");
        let canonical = self.validate_path(path)?;
        let utf8_path = Utf8Path::new(canonical.to_str().unwrap_or(path));

        let source = parser::read_file(utf8_path)?;
        let (tree, lang_id) = parser::parse_file(utf8_path, &source)?;
        let root = tree.root_node();

        let edges = imports::extract_imports(root, &source, lang_id)?;
        let language = lang_id.to_string();

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

    /// ソースファイルを指定ルールで lint する。
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

    /// 複数ファイルを横断してシンボル参照を検索する。
    pub fn find_references(&self, name: &str, dir: &str, glob: Option<&str>) -> Result<RefsResult> {
        debug!(name = name, dir = dir, glob = ?glob, "find_references called");
        let canonical_dir = self.validate_dir(dir)?;

        let references = refs::find_references(name, &canonical_dir, glob)?;

        // 絶対パスを `dir` 基準の相対パスへ変換する。
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

    /// 複数シンボルの参照をバッチで横断検索する。
    pub fn find_references_batch(
        &self,
        names: &[String],
        dir: &str,
        glob: Option<&str>,
    ) -> Result<Vec<RefsResult>> {
        debug!(names = ?names, dir = dir, glob = ?glob, "find_references_batch called");
        let canonical_dir = self.validate_dir(dir)?;

        let batch = refs::find_references_batch(names, &canonical_dir, glob)?;

        // 入力順を保ったまま `Vec<RefsResult>` に変換し、パスも相対化する。
        let results: Vec<RefsResult> = names
            .iter()
            .map(|name| {
                let references = batch.get(name).cloned().unwrap_or_default();
                let references = relativize_paths(references, &canonical_dir);
                RefsResult {
                    symbol: name.clone(),
                    references,
                }
            })
            .collect();

        debug!(
            names = ?names,
            dir = dir,
            total_refs = results.iter().map(|r| r.references.len()).sum::<usize>(),
            "find_references_batch completed"
        );
        Ok(results)
    }

    /// unified diff がコードベースへ与える影響を解析する。
    pub fn analyze_context(&self, diff: &str, dir: &str) -> Result<ContextResult> {
        let mut changes = Vec::new();
        self.analyze_context_streaming(diff, dir, |impact| {
            changes.push(impact);
            Ok(())
        })?;
        Ok(ContextResult { changes })
    }

    /// unified diff の影響を `FileImpact` 1 件ずつ callback に渡す streaming API。
    ///
    /// CLI 層で JSON を 1 件ずつ stdout に書き出せば、`Vec<FileImpact>` を全件保持する
    /// ことによる数 GB 級のピーク RSS を排除できる。`analyze_context` はこの薄い wrapper。
    pub fn analyze_context_streaming<F>(
        &self,
        diff: &str,
        dir: &str,
        mut on_file_impact: F,
    ) -> Result<()>
    where
        F: FnMut(crate::models::impact::FileImpact) -> Result<()>,
    {
        debug!(
            dir = dir,
            diff_bytes = diff.len(),
            "analyze_context_streaming called"
        );
        let canonical_dir = self.validate_dir(dir)?;
        self.validate_input_size(diff)?;

        let mut changes_count = 0usize;
        let mut callers_count = 0usize;
        let mut affected_count = 0usize;

        impact::analyze_impact_streaming(diff, &canonical_dir, |mut impact| {
            // impacted_callers 内の絶対パスを相対パスへ変換する。
            for caller in &mut impact.impacted_callers {
                if let Ok(rel) = std::path::Path::new(&caller.path).strip_prefix(&canonical_dir) {
                    caller.path = rel.to_string_lossy().to_string();
                }
            }
            changes_count += 1;
            affected_count += impact.affected_symbols.len();
            callers_count += impact.impacted_callers.len();
            on_file_impact(impact)
        })?;

        debug!(
            dir = dir,
            changes = changes_count,
            total_affected = affected_count,
            total_callers = callers_count,
            "analyze_context_streaming completed"
        );
        Ok(())
    }

    /// git 履歴から共変更パターンを解析する。
    pub fn analyze_cochange(&self, dir: &str, opts: &CoChangeOptions) -> Result<CoChangeResult> {
        debug!(
            dir = dir,
            lookback = opts.lookback,
            min_confidence = opts.min_confidence,
            min_samples = opts.min_samples,
            max_files_per_commit = opts.max_files_per_commit,
            bounded_by_merge_base = opts.bounded_by_merge_base,
            skip_deleted_files = opts.skip_deleted_files,
            filter_file = ?opts.filter_file,
            "analyze_cochange called"
        );
        // パラメータを検証する。
        const MAX_LOOKBACK: usize = 10_000;
        if opts.lookback == 0 || opts.lookback > MAX_LOOKBACK {
            bail!(AstroError::new(
                ErrorCode::InvalidRequest,
                format!("lookback must be 1..={MAX_LOOKBACK}, got {}", opts.lookback),
            ));
        }
        if !opts.min_confidence.is_finite() || !(0.0..=1.0).contains(&opts.min_confidence) {
            bail!(AstroError::new(
                ErrorCode::InvalidRequest,
                format!(
                    "min_confidence must be a finite value in [0.0, 1.0], got {}",
                    opts.min_confidence
                ),
            ));
        }
        if opts.max_files_per_commit == 0 {
            bail!(AstroError::new(
                ErrorCode::InvalidRequest,
                "max_files_per_commit must be >= 1".to_string(),
            ));
        }

        let canonical_dir = self.validate_dir(dir)?;
        let dir_str = canonical_dir.to_string_lossy();

        let result = if opts.blame {
            crate::engine::cochange::analyze_cochange_blame(&dir_str, opts)?
        } else {
            crate::engine::cochange::analyze_cochange(&dir_str, opts)?
        };
        debug!(
            dir = dir,
            blame = opts.blame,
            entries = result.entries.len(),
            commits_analyzed = result.commits_analyzed,
            "analyze_cochange completed"
        );
        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// パス補助関数
// ---------------------------------------------------------------------------

/// 絶対パスを `dir` 基準の相対パスへ変換する。
/// `dir` 配下でないパスはそのまま残す。
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
// 診断情報ヘルパー（AppService の全コード経路で共有）
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 制限なしサービスの生成
    #[test]
    fn new_creates_unrestricted_service() {
        let service = AppService::new();
        assert!(service.workspace_root.is_none());
        assert_eq!(service.max_input_size, 0);
    }

    /// Default trait が new() と同じ結果を返す
    #[test]
    fn default_equals_new() {
        let service = AppService::default();
        assert!(service.workspace_root.is_none());
    }

    /// sandboxed で有効なディレクトリを指定した場合
    #[test]
    fn sandboxed_valid_directory() {
        let dir = tempfile::tempdir().unwrap();
        let service = AppService::sandboxed(dir.path().to_path_buf()).unwrap();
        assert!(service.workspace_root.is_some());
        assert_eq!(service.max_input_size, 100 * 1024 * 1024);
    }

    /// sandboxed で存在しないパスを指定するとエラー
    #[test]
    fn sandboxed_nonexistent_path() {
        let result = AppService::sandboxed(PathBuf::from("/nonexistent/path"));
        assert!(result.is_err());
    }

    /// sandboxed でファイル（ディレクトリでない）を指定するとエラー
    #[test]
    fn sandboxed_file_not_directory() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        std::fs::write(&file_path, "hello").unwrap();
        let result = AppService::sandboxed(file_path);
        assert!(result.is_err());
    }

    /// validate_path でワークスペース外のパスを拒否する
    #[test]
    fn validate_path_rejects_outside_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let service = AppService::sandboxed(dir.path().to_path_buf()).unwrap();
        // /tmp 自体はワークスペース外
        let result = service.validate_path("/etc/passwd");
        assert!(result.is_err());
    }

    /// validate_path で存在しないファイルをエラーにする
    #[test]
    fn validate_path_rejects_nonexistent() {
        let service = AppService::new();
        let result = service.validate_path("/nonexistent/file.rs");
        assert!(result.is_err());
    }

    /// validate_dir でファイルパスを拒否する
    #[test]
    fn validate_dir_rejects_file() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        std::fs::write(&file_path, "hello").unwrap();
        let service = AppService::new();
        let result = service.validate_dir(file_path.to_str().unwrap());
        assert!(result.is_err());
    }

    /// validate_input_size で上限超過を拒否する
    #[test]
    fn validate_input_size_rejects_oversized() {
        let dir = tempfile::tempdir().unwrap();
        let service = AppService::sandboxed(dir.path().to_path_buf()).unwrap();
        let large = "x".repeat(100 * 1024 * 1024 + 1);
        let result = service.validate_input_size(&large);
        assert!(result.is_err());
    }

    /// validate_input_size で無制限モード（max_input_size=0）では何でも許可
    #[test]
    fn validate_input_size_unlimited() {
        let service = AppService::new();
        let large = "x".repeat(200 * 1024 * 1024);
        let result = service.validate_input_size(&large);
        assert!(result.is_ok());
    }

    /// relativize_paths でディレクトリ内のパスを相対化する
    #[test]
    fn relativize_paths_converts_absolute() {
        use crate::models::reference::SymbolReference;
        let dir = std::path::Path::new("/home/user/project");
        let refs = vec![SymbolReference {
            path: "/home/user/project/src/main.rs".to_string(),
            line: 10,
            column: 5,
            context: None,
            kind: None,
        }];
        let result = relativize_paths(refs, dir);
        assert_eq!(result[0].path, "src/main.rs");
    }

    /// relativize_paths でディレクトリ外のパスはそのまま
    #[test]
    fn relativize_paths_keeps_outside() {
        use crate::models::reference::SymbolReference;
        let dir = std::path::Path::new("/home/user/project");
        let refs = vec![SymbolReference {
            path: "/other/path/file.rs".to_string(),
            line: 1,
            column: 0,
            context: None,
            kind: None,
        }];
        let result = relativize_paths(refs, dir);
        assert_eq!(result[0].path, "/other/path/file.rs");
    }

    /// extract_symbols で有効な Rust ファイルからシンボルを抽出する
    #[test]
    fn extract_symbols_from_rust_file() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.rs");
        std::fs::write(&file_path, "fn hello() {}\nstruct Foo {}").unwrap();
        let service = AppService::new();
        let result = service
            .extract_symbols(file_path.to_str().unwrap())
            .unwrap();
        let syms = result.symbols.unwrap();
        assert!(syms.iter().any(|s| s.name == "hello"));
        assert!(syms.iter().any(|s| s.name == "Foo"));
    }

    /// from_env で ASTRO_SIGHT_WORKSPACE が未設定の場合は無制限モード
    #[test]
    fn from_env_without_workspace() {
        // Rust 2024 では remove_var は unsafe
        unsafe { std::env::remove_var("ASTRO_SIGHT_WORKSPACE") };
        let service = AppService::from_env().unwrap();
        assert!(service.workspace_root.is_none());
    }
}
