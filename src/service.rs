use anyhow::{Result, bail};
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

use crate::cache::store::CacheStore;
use crate::engine::{
    calls, extractor, impact, imports, lexer, lint, parser, refs, snippet, symbols,
};
use crate::error::{AstroError, ErrorCode};
use crate::language::{DetectedLang, LangId};
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

/// `AppService` の tree-sitter 系操作が共有する、検証・読み込み・parse 済み入力。
///
/// パス境界チェックと 100MB 上限を各操作が個別実装しないよう、生成経路を 1 箇所に閉じる。
struct ParsedFile {
    source: parser::SourceBuf,
    tree: tree_sitter::Tree,
    lang_id: LangId,
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

    /// sandboxed/session/MCP モードでは、相対パスをワークスペースルート基準で解決する。
    /// CLI の無制限モードでは従来通りカレントディレクトリ基準にする。
    fn workspace_candidate(&self, path: &str) -> PathBuf {
        let candidate = Path::new(path);
        if let Some(root) = &self.workspace_root
            && candidate.is_relative()
        {
            return root.join(candidate);
        }
        candidate.to_path_buf()
    }

    /// ファイルパスを検証して正規化し、正規化済みパスを返す。
    fn validate_path(&self, path: &str) -> Result<PathBuf> {
        let candidate = self.workspace_candidate(path);
        let canonical = std::fs::canonicalize(&candidate).map_err(|_| {
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

    /// ファイルパスを検証し、UTF-8 として扱える `Utf8PathBuf` を返す。
    /// 非 UTF-8 のパスは fail-closed でエラーにする（境界チェック後の元パスフォールバックを避ける）。
    fn validate_path_utf8(&self, path: &str) -> Result<camino::Utf8PathBuf> {
        let canonical = self.validate_path(path)?;
        camino::Utf8PathBuf::try_from(canonical).map_err(|e| {
            AstroError::new(
                ErrorCode::InvalidRequest,
                format!("Path contains non-UTF-8 bytes: {}", e.as_path().display()),
            )
            .into()
        })
    }

    /// ディレクトリパスを検証して正規化し、正規化済みパスを返す。
    fn validate_dir(&self, dir: &str) -> Result<PathBuf> {
        let candidate = self.workspace_candidate(dir);
        let canonical = std::fs::canonicalize(&candidate).map_err(|_| {
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

    /// tree-sitter を使う単一ファイル操作の共通ロード処理。
    fn load_parsed_file(&self, path: &str) -> Result<ParsedFile> {
        let utf8_path = self.validate_path_utf8(path)?;
        let source = parser::read_file(&utf8_path)?;
        let (tree, lang_id) = parser::parse_file(&utf8_path, &source)?;
        Ok(ParsedFile {
            source,
            tree,
            lang_id,
        })
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
        let ParsedFile {
            source,
            tree,
            lang_id,
        } = self.load_parsed_file(p.path)?;
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
    ///
    /// `LangId::is_lexer_only()` な言語 (現状 Xojo) は手書き lexer で抽出する。
    /// tree-sitter は使わないためメモリ消費は入力サイズに対して線形ではなく定数倍に近い。
    pub fn extract_symbols(&self, path: &str) -> Result<AstgenResponse> {
        self.extract_symbols_with_query(path, None)
    }

    /// カスタム tree-sitter クエリ付きのシンボル抽出 (`symbols --query` / session の
    /// `query` フィールド)。`query` が `None` なら built-in クエリ (従来動作)。
    /// 不正なクエリ・未知 capture は `INVALID_REQUEST` を返し、silent no-op にしない。
    pub fn extract_symbols_with_query(
        &self,
        path: &str,
        query: Option<&str>,
    ) -> Result<AstgenResponse> {
        debug!(path = path, query = ?query, "extract_symbols called");
        let utf8_path_buf = self.validate_path_utf8(path)?;
        let utf8_path = utf8_path_buf.as_path();

        let source = parser::read_file(utf8_path)?;

        // 言語検出 (shebang fallback と .h の C/C++ 判定を考慮)。
        let lang_id = parser::detect_lang(utf8_path, &source)?;

        // lexer-only 言語は tree-sitter を使わず手書き lexer で抽出する。
        if let DetectedLang::LexerOnly(lexer_lang) = lang_id.detected() {
            if query.is_some() {
                return Err(crate::error::AstroError::new(
                    crate::error::ErrorCode::InvalidRequest,
                    format!("--query is not supported for lexer-only language: {lang_id}"),
                )
                .into());
            }
            let syms = lexer::extract_symbols(&source, lexer_lang);
            let location = LocationKey::file_only(path);
            let mut response = AstgenResponse::success(location, lang_id);
            response.hash = Some(CacheStore::hash(&source));
            response.symbols = Some(syms);
            // lexer 経路では tree-sitter 診断 (構文エラー等) は出ない。
            debug!(
                path = path,
                language = ?lang_id,
                symbols = response.symbols.as_ref().map(|s| s.len()).unwrap_or(0),
                "extract_symbols completed (lexer)"
            );
            return Ok(response);
        }

        let tree = parser::parse_source(&source, lang_id)?;
        let root = tree.root_node();

        let syms = match query {
            Some(q) => symbols::extract_symbols_with_custom_query(root, &source, lang_id, q)?,
            None => symbols::extract_symbols(root, &source, lang_id)?,
        };

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
        let ParsedFile {
            source,
            tree,
            lang_id,
        } = self.load_parsed_file(path)?;
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
        let graph = self.extract_calls(path, function)?;
        let result =
            crate::engine::sequence::generate_sequence_diagram(&graph.calls, &graph.language);
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
        let ParsedFile {
            source,
            tree,
            lang_id,
        } = self.load_parsed_file(path)?;
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
        let ParsedFile {
            source,
            tree,
            lang_id,
        } = self.load_parsed_file(path)?;
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
        self.find_references_with_generated(name, dir, glob, false)
    }

    /// Reference search with an explicit generated-file policy.
    pub fn find_references_with_generated(
        &self,
        name: &str,
        dir: &str,
        glob: Option<&str>,
        include_generated: bool,
    ) -> Result<RefsResult> {
        debug!(name = name, dir = dir, glob = ?glob, "find_references called");
        let canonical_dir = self.validate_dir(dir)?;

        let (references, skipped) = refs::find_references_with_scan(
            name,
            &canonical_dir,
            glob,
            refs::FileScanOptions { include_generated },
        )?;

        // 絶対パスを `dir` 基準の相対パスへ変換する。
        let references = relativize_paths(references, &canonical_dir);

        let result = RefsResult {
            symbol: name.to_string(),
            references,
            skipped,
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
        self.find_references_batch_with_generated(names, dir, glob, false)
    }

    /// Batch reference search with shared generated-file omission metadata.
    ///
    /// The metadata is attached once to the first result. This preserves the
    /// existing array/NDJSON shape and avoids repeating the same path list for
    /// every requested symbol.
    pub fn find_references_batch_with_generated(
        &self,
        names: &[String],
        dir: &str,
        glob: Option<&str>,
        include_generated: bool,
    ) -> Result<Vec<RefsResult>> {
        debug!(names = ?names, dir = dir, glob = ?glob, "find_references_batch called");
        let canonical_dir = self.validate_dir(dir)?;

        let (batch, skipped) = refs::find_references_batch_with_scan(
            names,
            &canonical_dir,
            glob,
            refs::FileScanOptions { include_generated },
        )?;

        // 入力順を保ったまま `Vec<RefsResult>` に変換し、パスも相対化する。
        let mut results: Vec<RefsResult> = names
            .iter()
            .map(|name| {
                let references = batch.get(name).cloned().unwrap_or_default();
                let references = relativize_paths(references, &canonical_dir);
                RefsResult {
                    symbol: name.clone(),
                    references,
                    skipped: None,
                }
            })
            .collect();
        if let Some(first) = results.first_mut() {
            first.skipped = skipped;
        }

        debug!(
            names = ?names,
            dir = dir,
            total_refs = results.iter().map(|r| r.references.len()).sum::<usize>(),
            "find_references_batch completed"
        );
        Ok(results)
    }

    /// unified diff がコードベースへ与える影響を解析する。
    ///
    /// `options.exclude_dirs` / `options.exclude_globs` は Pass2 cross-file 検索
    /// から追加で除外したい対象を指定する (固定の `IMPACT_DEFAULT_EXCLUDED_DIRS`
    /// にマージして適用)。
    pub fn analyze_context(
        &self,
        diff: &str,
        dir: &str,
        options: &crate::models::impact::ContextAnalysisOptions,
    ) -> Result<ContextResult> {
        let mut changes = Vec::new();
        self.analyze_context_streaming(diff, dir, options, |impact| {
            changes.push(impact);
            Ok(())
        })?;
        Ok(ContextResult {
            changes,
            skipped: None,
            // 打ち切りは diff 取得層 (git_input) が持つ情報なので、CLI 側で結果に載せる。
            truncations: Vec::new(),
        })
    }

    /// context 解析の入力検証だけを行う。
    ///
    /// streaming CLI は stdout へ JSON prefix を書き始める前にこれを呼び、
    /// 入力エラー時にも壊れていない JSON エラーを返せるようにする。
    /// `options.exclude_globs` の構文も先行検証して、streaming JSON の prefix を
    /// 出した後に silent empty 結果を返すのを防ぐ。
    pub fn validate_context_inputs(
        &self,
        diff: &str,
        dir: &str,
        options: &crate::models::impact::ContextAnalysisOptions,
    ) -> Result<()> {
        self.validate_dir(dir)?;
        self.validate_input_size(diff)?;
        validate_exclude_globs(&options.exclude_globs)
    }

    /// unified diff の影響を `FileImpact` 1 件ずつ callback に渡す streaming API。
    ///
    /// CLI 層で JSON を 1 件ずつ stdout に書き出せば、`Vec<FileImpact>` を全件保持する
    /// ことによる数 GB 級のピーク RSS を排除できる。`options.exclude_globs` 等の
    /// 構文不正は実行前に弾く。
    pub fn analyze_context_streaming<F>(
        &self,
        diff: &str,
        dir: &str,
        options: &crate::models::impact::ContextAnalysisOptions,
        mut on_file_impact: F,
    ) -> Result<()>
    where
        F: FnMut(crate::models::impact::FileImpact) -> Result<()>,
    {
        debug!(
            dir = dir,
            diff_bytes = diff.len(),
            extra_exclude_dirs = options.exclude_dirs.len(),
            extra_exclude_globs = options.exclude_globs.len(),
            "analyze_context_streaming called"
        );
        let canonical_dir = self.validate_dir(dir)?;
        self.validate_input_size(diff)?;
        validate_exclude_globs(&options.exclude_globs)?;

        let mut changes_count = 0usize;
        let mut callers_count = 0usize;
        let mut affected_count = 0usize;

        impact::analyze_impact_streaming(diff, &canonical_dir, options, |mut impact| {
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

    /// blame ベースの共変更パターンを解析する。
    pub fn analyze_cochange(&self, dir: &str, opts: &CoChangeOptions) -> Result<CoChangeResult> {
        debug!(
            dir = dir,
            source_files = opts.source_files.len(),
            base = ?opts.base,
            min_confidence = opts.min_confidence,
            min_samples = opts.min_samples,
            max_files_per_commit = opts.max_files_per_commit,
            "analyze_cochange called"
        );
        if !opts.min_confidence.is_finite() || !(0.0..=1.0).contains(&opts.min_confidence) {
            bail!(AstroError::new(
                ErrorCode::InvalidRequest,
                format!(
                    "min_confidence must be a finite value in [0.0, 1.0], got {}",
                    opts.min_confidence
                ),
            ));
        }
        if !opts.min_score.is_finite() || !(0.0..=1.0).contains(&opts.min_score) {
            bail!(AstroError::new(
                ErrorCode::InvalidRequest,
                format!(
                    "min_score must be a finite value in [0.0, 1.0], got {}",
                    opts.min_score
                ),
            ));
        }
        if opts.max_files_per_commit == 0 {
            bail!(AstroError::new(
                ErrorCode::InvalidRequest,
                "max_files_per_commit must be >= 1".to_string(),
            ));
        }
        if !opts.smoothing_alpha.is_finite() || opts.smoothing_alpha < 0.0 {
            bail!(AstroError::new(
                ErrorCode::InvalidRequest,
                format!(
                    "smoothing_alpha must be a finite non-negative value, got {}",
                    opts.smoothing_alpha
                ),
            ));
        }
        if !opts.smoothing_beta.is_finite() || opts.smoothing_beta < 0.0 {
            bail!(AstroError::new(
                ErrorCode::InvalidRequest,
                format!(
                    "smoothing_beta must be a finite non-negative value, got {}",
                    opts.smoothing_beta
                ),
            ));
        }

        let canonical_dir = self.validate_dir(dir)?;

        // source_files は git の pathspec として `--` の後ろに渡るためオプション注入は
        // 起きないが、`../` を含むと canonical_dir (= サンドボックス配下に検証済み) の
        // 外にある同一リポジトリ内のファイルを blame できてしまう。dir 検証と同じ場所で
        // 「dir 配下の相対パス」に閉じる (他のパス系 API と検証系列を揃える)。
        //
        // 検証だけでなく**正規化**もここで行う。engine 側の起点除外は生の文字列一致だが、
        // 候補は `git diff-tree --name-only` 由来の正規形なので、`./src/a.rs` や
        // `src//a.rs` のような同値表記だと起点自身が候補として残り
        // 「自分自身との共変更 confidence 1.0」が最上位に出る。MCP の cochange_analyze は
        // エージェントが source_files を組み立てるため実運用で踏みやすい。
        // (`is_contained_relative` 側で CurDir を弾く案は採れない — 既存テストが
        // `./src/main.rs` を正当な入力として固定している。)
        //
        // 上限 (`max_source_files`) はここで **正規化後の unique 件数**に対して掛ける。
        // engine 側の判定より前に置くのは、暴走防止の上限が「大量確保と全件比較を
        // 終えてから」効くのでは意味が無いため。dedup は `Vec::contains` の O(N²) では
        // なく `HashSet` で行い、`Vec` は入力順の保持だけに使う。
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut normalized_sources: Vec<String> = Vec::new();
        for f in &opts.source_files {
            let Some(normalized) = normalize_contained_relative(f) else {
                bail!(AstroError::new(
                    ErrorCode::PathOutOfBounds,
                    format!(
                        "source_files must be a relative path under --dir \
                         (no '..', no absolute or drive-qualified prefix): {f}"
                    ),
                ));
            };
            // 同値表記が複数指定された場合は先勝ちで畳む (出力順は入力順のまま)。
            if !seen.insert(normalized.clone()) {
                continue;
            }
            if opts.max_source_files > 0 && normalized_sources.len() >= opts.max_source_files {
                bail!(AstroError::new(
                    ErrorCode::InvalidRequest,
                    format!(
                        "source_files count exceeds --max-source-files limit {}; \
                         narrow --paths or raise the limit explicitly",
                        opts.max_source_files,
                    ),
                ));
            }
            normalized_sources.push(normalized);
        }

        let dir_str = canonical_dir.to_string_lossy();

        let normalized_opts = CoChangeOptions {
            source_files: normalized_sources,
            ..opts.clone()
        };
        let result = crate::engine::cochange::analyze_cochange(&dir_str, &normalized_opts)?;
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
// 入力検証ヘルパー
// ---------------------------------------------------------------------------

/// `--dir` 基準の相対パスとして安全かを字句レベルで判定する。
///
/// `..` (ParentDir) / `/` (RootDir) / Windows のドライブ修飾 (Prefix, `C:foo` 含む) を
/// `Path::components()` の 1 判定でまとめて弾く。文字列 prefix 判定では
/// `C:foo` のような drive-relative 形式を取りこぼすため components を使う。
///
/// blame は base リビジョン側の履歴を辿るので、working tree に実体が無い
/// (base 時点にしか存在しない) ファイルも正当な入力になる。したがって
/// canonicalize による実在確認は行わない。
/// `normalize_contained_relative` の真偽値ラッパー。
///
/// 本番経路は正規化結果を必要とするため `normalize_contained_relative` を直接呼ぶ。
/// 「安全なパスか」だけを問う既存テストの可読性を保つために test 専用で残す
/// (本番から参照が無いので `#[cfg(test)]` を外すと dead_code で clippy が落ちる)。
#[cfg(test)]
fn is_contained_relative(p: &str) -> bool {
    normalize_contained_relative(p).is_some()
}

/// `--dir` 基準の相対パスを検証しつつ正規形へ畳む。安全でなければ `None`。
///
/// `.` (CurDir) と連続スラッシュを落とし、残った `Normal` セグメントを `/` で再結合する。
/// 検証と正規化を 1 関数にまとめるのは、「検証は通したが正規化を忘れた経路」が生まれる
/// のを防ぐため (engine 側の起点除外は生の文字列一致なので、正規化漏れは
/// 「自分自身との共変更」という誤検出になって表に出る)。
///
/// `..` (ParentDir) / `/` (RootDir) / Windows のドライブ修飾 (Prefix, `C:foo` 含む) と、
/// 正規化した結果が空になる入力 (`.` / `./` 等) は `None`。
fn normalize_contained_relative(p: &str) -> Option<String> {
    use std::path::Component;
    if p.is_empty() {
        return None;
    }
    let mut segments: Vec<&str> = Vec::new();
    for component in Path::new(p).components() {
        match component {
            Component::Normal(seg) => segments.push(seg.to_str()?),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if segments.is_empty() {
        return None;
    }
    Some(segments.join("/"))
}

/// `--exclude-glob` の構文を `ignore::overrides::OverrideBuilder` で先行検証する。
///
/// 不正なパターンや空文字を analyze_context 開始前にエラー化することで、streaming
/// CLI が JSON prefix を出した後に silent empty 結果を返すのを防ぐ。
fn validate_exclude_globs(globs: &[String]) -> Result<()> {
    if globs.is_empty() {
        return Ok(());
    }
    let mut ob = ignore::overrides::OverrideBuilder::new(".");
    for g in globs {
        if g.is_empty() {
            bail!(AstroError::new(
                ErrorCode::InvalidRequest,
                "exclude-glob must not be empty".to_string(),
            ));
        }
        let negated = if g.starts_with('!') {
            g.clone()
        } else {
            format!("!{g}")
        };
        ob.add(&negated).map_err(|e| {
            anyhow::anyhow!(AstroError::new(
                ErrorCode::InvalidRequest,
                format!("invalid exclude-glob '{g}': {e}"),
            ))
        })?;
    }
    Ok(())
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

    /// `normalize_contained_relative` は検証と正規化を同時に行う。
    ///
    /// engine 側の起点除外は生の文字列一致だが、候補は `git diff-tree --name-only` 由来の
    /// 正規形なので、`./src/a.rs` や `src//a.rs` を素通しすると起点自身が候補に残り
    /// 「自分自身との共変更 confidence 1.0」が最上位に出る。
    #[test]
    fn normalize_contained_relative_folds_equivalent_spellings() {
        // 同値表記はすべて同じ正規形へ畳まれる。
        for spelling in [
            "src/a.rs",
            "./src/a.rs",
            "src//a.rs",
            "src/./a.rs",
            "./src/./a.rs",
        ] {
            assert_eq!(
                normalize_contained_relative(spelling).as_deref(),
                Some("src/a.rs"),
                "{spelling} は src/a.rs へ正規化されること"
            );
        }
        // 対照: 安全でない入力は従来どおり None (= 検証エラー)。
        // Windows のドライブ修飾は Prefix コンポーネントになる platform でのみ弾けるため、
        // ここでは platform 非依存のケースだけを見る
        // (`is_contained_relative_rejects_windows_drive_paths` が cfg(windows) で担当)。
        for bad in ["", ".", "./", "..", "../secret.rs", "/etc/passwd"] {
            assert_eq!(
                normalize_contained_relative(bad),
                None,
                "{bad:?} は拒否されること"
            );
        }
        // 対照: 正規化しても中身が変わらない入力はそのまま。
        assert_eq!(
            normalize_contained_relative("main.rs").as_deref(),
            Some("main.rs")
        );
    }

    #[test]
    fn is_contained_relative_accepts_plain_relative_paths() {
        assert!(is_contained_relative("src/main.rs"));
        assert!(is_contained_relative("main.rs"));
        // `./` 始まりは CurDir コンポーネントなので許可する
        assert!(is_contained_relative("./src/main.rs"));
        assert!(is_contained_relative("a/./b.rs"));
        // 名前に `..` を含むだけのパスは ParentDir ではない
        assert!(is_contained_relative("src/..hidden.rs"));
        assert!(is_contained_relative("src/a..b.rs"));
    }

    #[test]
    fn is_contained_relative_rejects_escapes() {
        // 空文字
        assert!(!is_contained_relative(""));
        // 親ディレクトリ参照 (先頭・途中いずれも)
        assert!(!is_contained_relative(".."));
        assert!(!is_contained_relative("../secret.rs"));
        assert!(!is_contained_relative("src/../../secret.rs"));
        // POSIX 絶対パス
        assert!(!is_contained_relative("/etc/passwd"));
    }

    #[cfg(windows)]
    #[test]
    fn is_contained_relative_rejects_windows_drive_paths() {
        // ドライブ修飾は Prefix コンポーネントになる。`C:foo` (drive-relative) も
        // Path::join が self を置換するため拒否する必要がある。
        assert!(!is_contained_relative(r"C:\Windows\System32"));
        assert!(!is_contained_relative("C:/Windows/System32"));
        assert!(!is_contained_relative("C:foo"));
        assert!(!is_contained_relative(r"\\server\share\x"));
    }

    /// sandbox 内から `../` で外部ファイルを blame できてしまう抜け道を塞ぐ。
    /// dir だけを検証していた頃は git の pathspec が cwd 基準で解決され、
    /// dir の外にある同一リポジトリ内ファイルの存在有無・共変更が漏れていた。
    #[test]
    fn cochange_rejects_source_files_outside_dir() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();

        let service = AppService::sandboxed(sub.clone()).unwrap();
        let opts = crate::models::cochange::CoChangeOptions {
            source_files: vec!["../outside.rs".to_string()],
            ..Default::default()
        };
        let err = service
            .analyze_cochange(".", &opts)
            .expect_err("../ を含む source_files は拒否されるべき");
        assert!(
            err.to_string().contains("PATH_OUT_OF_BOUNDS"),
            "PathOutOfBounds で失敗すべき: {err}"
        );
    }

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
            confidence: None,
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
            confidence: None,
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

    /// validate_path_utf8 で正常 UTF-8 パスは Utf8PathBuf を返す
    #[test]
    fn validate_path_utf8_accepts_valid_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("ok.rs");
        std::fs::write(&file_path, "fn main() {}").unwrap();
        let service = AppService::new();
        let result = service.validate_path_utf8(file_path.to_str().unwrap());
        assert!(result.is_ok());
    }

    /// validate_path_utf8 で存在しないパスは検証段階でエラー
    /// (validate_path 経由のチェックが先行することを担保)
    #[test]
    fn validate_path_utf8_rejects_nonexistent() {
        let service = AppService::new();
        let result = service.validate_path_utf8("/nonexistent/no/such.rs");
        assert!(result.is_err());
    }

    /// validate_path_utf8 でサンドボックス外のパスは PathOutOfBounds で拒否される
    /// (サンドボックスモードの境界保護が UTF-8 化前に効くことを保証)
    #[test]
    fn validate_path_utf8_rejects_outside_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        let service = AppService::sandboxed(dir.path().to_path_buf()).unwrap();
        // /etc/passwd は通常存在し、サンドボックス外
        let result = service.validate_path_utf8("/etc/passwd");
        assert!(result.is_err());
    }

    #[test]
    fn sandboxed_relative_file_path_is_workspace_relative() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("workspace_only");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(src_dir.join("lib.rs"), "pub fn workspace_symbol() {}\n").unwrap();

        let service = AppService::sandboxed(dir.path().to_path_buf()).unwrap();
        let result = service.extract_symbols("workspace_only/lib.rs");

        assert!(
            result.is_ok(),
            "相対ファイルパスはプロセス cwd ではなくワークスペース基準で解決されるべき"
        );
    }

    #[test]
    fn sandboxed_relative_dir_is_workspace_relative() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("only_in_workspace.rs"), "fn needle() {}\n").unwrap();

        let service = AppService::sandboxed(dir.path().to_path_buf()).unwrap();
        let refs = service
            .find_references("needle", ".", Some("**/*.rs"))
            .expect("ワークスペース相対の . を検索できるべき");

        assert_eq!(refs.references.len(), 1);
        assert_eq!(refs.references[0].path, "only_in_workspace.rs");
    }
}
