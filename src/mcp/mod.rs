use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::schemars;
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_router};

use schemars::JsonSchema;
use serde::Deserialize;

use crate::doctor;
use crate::service::{AppService, AstParams};

// ---------------------------------------------------------------------------
// Tool parameter structs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AstExtractParams {
    /// Path to the source file
    pub path: String,
    /// Line number (0-indexed)
    #[serde(default)]
    pub line: Option<usize>,
    /// Column number (0-indexed)
    #[serde(default)]
    pub col: Option<usize>,
    /// End line (for range extraction)
    #[serde(default)]
    pub end_line: Option<usize>,
    /// End column (for range extraction)
    #[serde(default)]
    pub end_col: Option<usize>,
    /// Max depth of AST traversal (default: 3)
    #[serde(default = "default_depth")]
    pub depth: usize,
    /// Number of context lines in snippet (default: 3)
    #[serde(default = "default_context_lines")]
    pub context_lines: usize,
}

fn default_depth() -> usize {
    3
}

fn default_context_lines() -> usize {
    3
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SymbolsExtractParams {
    /// Path to the source file
    pub path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CallsExtractParams {
    /// Path to the source file
    pub path: String,
    /// Filter to a specific function name
    #[serde(default)]
    pub function: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RefsSearchParams {
    /// Symbol name to search for
    pub name: String,
    /// Directory to search in
    pub dir: String,
    /// Glob pattern to filter files (e.g. "**/*.rs")
    #[serde(default)]
    pub glob: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ContextAnalyzeParams {
    /// Unified diff text
    pub diff: String,
    /// Workspace directory
    pub dir: String,
}

// ---------------------------------------------------------------------------
// MCP Server
// ---------------------------------------------------------------------------

#[derive(Clone)]
#[allow(dead_code)]
pub struct AstroSightServer {
    tool_router: ToolRouter<Self>,
    service: std::sync::Arc<AppService>,
}

impl Default for AstroSightServer {
    fn default() -> Self {
        Self::new()
    }
}

#[tool_router]
impl AstroSightServer {
    pub fn new() -> Self {
        let cwd = std::env::current_dir().unwrap_or_default();
        let service = AppService::sandboxed(cwd).unwrap_or_else(|_| AppService::new());
        Self {
            tool_router: Self::tool_router(),
            service: std::sync::Arc::new(service),
        }
    }

    #[tool(
        name = "ast_extract",
        description = "Extract AST fragment at a given position or range in a source file"
    )]
    async fn ast_extract(
        &self,
        params: Parameters<AstExtractParams>,
    ) -> Result<CallToolResult, McpError> {
        let p = params.0;
        let ast_params = AstParams {
            path: &p.path,
            line: p.line,
            col: p.col,
            end_line: p.end_line,
            end_col: p.end_col,
            depth: p.depth,
            context_lines: p.context_lines,
        };
        Self::to_tool_result(self.service.extract_ast(&ast_params))
    }

    #[tool(
        name = "symbols_extract",
        description = "Extract symbols (functions, classes, etc.) from a source file"
    )]
    async fn symbols_extract(
        &self,
        params: Parameters<SymbolsExtractParams>,
    ) -> Result<CallToolResult, McpError> {
        Self::to_tool_result(self.service.extract_symbols(&params.0.path))
    }

    #[tool(
        name = "calls_extract",
        description = "Extract call graph from a source file"
    )]
    async fn calls_extract(
        &self,
        params: Parameters<CallsExtractParams>,
    ) -> Result<CallToolResult, McpError> {
        let p = params.0;
        Self::to_tool_result(self.service.extract_calls(&p.path, p.function.as_deref()))
    }

    #[tool(
        name = "refs_search",
        description = "Search for symbol references across files in a directory"
    )]
    async fn refs_search(
        &self,
        params: Parameters<RefsSearchParams>,
    ) -> Result<CallToolResult, McpError> {
        let p = params.0;
        Self::to_tool_result(
            self.service
                .find_references(&p.name, &p.dir, p.glob.as_deref()),
        )
    }

    #[tool(
        name = "context_analyze",
        description = "Analyze the impact of a unified diff on the codebase"
    )]
    async fn context_analyze(
        &self,
        params: Parameters<ContextAnalyzeParams>,
    ) -> Result<CallToolResult, McpError> {
        let p = params.0;
        Self::to_tool_result(self.service.analyze_context(&p.diff, &p.dir))
    }

    #[tool(
        name = "doctor",
        description = "Check tool availability and supported languages"
    )]
    async fn doctor_tool(&self) -> Result<CallToolResult, McpError> {
        let report = doctor::run_doctor();
        let json = serde_json::to_string(&report)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }
}

// ---------------------------------------------------------------------------
// Helper
// ---------------------------------------------------------------------------

impl AstroSightServer {
    fn to_tool_result<T: serde::Serialize>(
        result: anyhow::Result<T>,
    ) -> Result<CallToolResult, McpError> {
        match result {
            Ok(value) => {
                let json = serde_json::to_string(&value)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                Ok(CallToolResult::success(vec![Content::text(json)]))
            }
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }
}

// ---------------------------------------------------------------------------
// ServerHandler implementation
// ---------------------------------------------------------------------------

impl ServerHandler for AstroSightServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::V_2024_11_05,
            capabilities: ServerCapabilities {
                tools: Some(ToolsCapability::default()),
                ..Default::default()
            },
            server_info: Implementation {
                name: "astro-sight".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }
}
