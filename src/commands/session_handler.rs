use anyhow::{Result, bail};

use crate::doctor;
use crate::error::{AstroError, ErrorCode};
use crate::models::cochange::CoChangeOptions;
use crate::service::{AppService, AstParams};

pub fn handle_request(
    service: &AppService,
    req: crate::models::request::AstgenRequest,
) -> Result<serde_json::Value> {
    use crate::models::request::Command;

    match req.command {
        Command::Ast => {
            let params = AstParams {
                path: &req.path,
                line: req.line,
                col: req.column,
                end_line: req.end_line,
                end_col: req.end_column,
                depth: req.depth.unwrap_or(3),
                context_lines: req.context_lines.unwrap_or(3),
            };
            let response = service.extract_ast(&params)?;
            Ok(serde_json::to_value(response)?)
        }
        Command::Symbols => {
            let response = service.extract_symbols_with_query(&req.path, req.query.as_deref())?;
            let compact = response.to_compact_symbols(false);
            Ok(serde_json::to_value(compact)?)
        }
        Command::Doctor => {
            let report = doctor::run_doctor();
            Ok(serde_json::to_value(report)?)
        }
        Command::Calls => {
            let result = service.extract_calls(&req.path, req.function.as_deref())?;
            Ok(serde_json::to_value(result.to_compact())?)
        }
        Command::Refs => {
            let dir = req.dir.as_deref().unwrap_or(".");
            if let Some(names) = &req.names {
                let filtered: Vec<String> = names
                    .iter()
                    .map(|name| name.trim().to_string())
                    .filter(|name| !name.is_empty())
                    .collect();
                if filtered.is_empty() {
                    return Err(AstroError::new(
                        ErrorCode::InvalidRequest,
                        "One of name or names is required",
                    )
                    .into());
                }
                let results = service.find_references_batch(&filtered, dir, req.glob.as_deref())?;
                Ok(serde_json::to_value(results)?)
            } else if let Some(name) = req.name.as_deref().map(str::trim).filter(|n| !n.is_empty())
            {
                let result = service.find_references(name, dir, req.glob.as_deref())?;
                Ok(serde_json::to_value(result)?)
            } else {
                Err(AstroError::new(
                    ErrorCode::InvalidRequest,
                    "One of name or names is required",
                )
                .into())
            }
        }
        Command::Context => {
            let dir = req.dir.as_deref().unwrap_or(".");
            let diff_input = req.diff.as_deref().unwrap_or("");
            let options = crate::models::impact::ContextAnalysisOptions {
                exclude_dirs: req.exclude_dirs.clone(),
                exclude_globs: req.exclude_globs.clone(),
            };
            let result = service.analyze_context(diff_input, dir, &options)?;
            Ok(serde_json::to_value(result)?)
        }
        Command::Imports => {
            let result = service.extract_imports(&req.path)?;
            Ok(serde_json::to_value(result)?)
        }
        Command::Lint => {
            let rules = req.rules.as_deref().unwrap_or(&[]);
            let result = service.lint_file(&req.path, rules)?;
            Ok(serde_json::to_value(result)?)
        }
        Command::Sequence => {
            let result = service.generate_sequence(&req.path, req.function.as_deref())?;
            Ok(serde_json::to_value(result)?)
        }
        Command::Cochange => {
            let dir = req.dir.as_deref().unwrap_or(".");
            let defaults = CoChangeOptions::default();
            let source_files = req.source_files.clone().unwrap_or_default();
            if source_files.is_empty() {
                bail!(crate::error::AstroError::new(
                    crate::error::ErrorCode::InvalidRequest,
                    "cochange (blame mode) requires source_files".to_string(),
                ));
            }
            let opts = CoChangeOptions {
                source_files,
                base: req.base.clone(),
                min_confidence: req.min_confidence.unwrap_or(defaults.min_confidence),
                min_samples: req.min_samples.unwrap_or(defaults.min_samples),
                max_files_per_commit: req
                    .max_files_per_commit
                    .unwrap_or(defaults.max_files_per_commit),
                ..defaults
            };
            let result = service.analyze_cochange(dir, &opts)?;
            Ok(serde_json::to_value(result)?)
        }
    }
}
