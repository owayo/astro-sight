//! プロジェクト辞書向けの識別子分割と、上限付きのシンボル抽出。
use crate::{
    engine::{
        generated::{self, GeneratedVerdict},
        gitattributes::AttributeResolver,
    },
    error::{AstroError, ErrorCode},
    models::symbol::Symbol,
    service::AppService,
};
use anyhow::Result;
use serde::Serialize;
use std::{
    path::{Component, Path},
    time::{Duration, Instant},
};

/// camelCase、頭字語、数字、区切り文字の境目で分ける。表記は変えない。
pub fn split_identifier(input: &str) -> Vec<String> {
    let chars: Vec<_> = input.char_indices().collect();
    let mut result = vec![];
    let mut start = None;
    for (i, (offset, c)) in chars.iter().copied().enumerate() {
        if !c.is_alphanumeric() {
            if let Some(start) = start.take() {
                result.push(input[start..offset].to_owned());
            }
            continue;
        }
        let previous = i.checked_sub(1).and_then(|i| chars.get(i)).map(|(_, c)| *c);
        let next = chars.get(i + 1).map(|(_, c)| *c);
        let boundary = start.is_some()
            && previous.is_some_and(|p| {
                (p.is_lowercase() && c.is_uppercase())
                    || (p.is_uppercase()
                        && c.is_uppercase()
                        && next.is_some_and(char::is_lowercase))
                    || (p.is_ascii_digit() != c.is_ascii_digit())
            });
        if boundary {
            if let Some(start) = start.replace(offset) {
                result.push(input[start..offset].to_owned());
            }
        } else if start.is_none() {
            start = Some(offset);
        }
    }
    if let Some(start) = start {
        result.push(input[start..].to_owned());
    }
    result
}
#[derive(Debug, Serialize)]
pub struct ScannedFile {
    pub path: String,
    pub symbols: Vec<Symbol>,
}
#[derive(Debug, Serialize)]
pub struct ScanReport {
    pub files: Vec<ScannedFile>,
    pub removed: Vec<String>,
    pub skipped_generated: usize,
    pub skipped_unsupported: usize,
    pub complete: bool,
    pub diagnostics: Vec<String>,
}
impl AppService {
    /// changed を指定した場合は、その相対パスだけを読み直す。削除も値で返す。
    pub fn scan_symbols(&self, dir: &str, changed: Option<&[String]>) -> Result<ScanReport> {
        let root = self.validate_dir(dir)?.canonicalize()?;
        let started = Instant::now();
        let mut report = ScanReport {
            files: vec![],
            removed: vec![],
            skipped_generated: 0,
            skipped_unsupported: 0,
            complete: true,
            diagnostics: vec![],
        };
        let mut files = vec![];
        if let Some(changed) = changed {
            for relative in changed {
                let path = Path::new(relative);
                if path.as_os_str().is_empty()
                    || path
                        .components()
                        .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
                {
                    return Err(AstroError::new(
                        ErrorCode::PathOutOfBounds,
                        "changed paths must stay inside the project",
                    )
                    .into());
                }
                let path = root.join(path);
                if path.try_exists()? {
                    files.push(path);
                } else {
                    let parent = path
                        .parent()
                        .ok_or_else(|| {
                            AstroError::new(ErrorCode::InvalidRequest, "missing parent")
                        })?
                        .canonicalize()?;
                    if !parent.starts_with(&root) {
                        return Err(AstroError::new(
                            ErrorCode::PathOutOfBounds,
                            "deleted path escapes project",
                        )
                        .into());
                    }
                    report.removed.push(relative.replace('\\', "/"));
                }
            }
        } else {
            for entry in ignore::WalkBuilder::new(&root).follow_links(false).build() {
                if started.elapsed() > Duration::from_secs(30) || files.len() >= 10_000 {
                    report.complete = false;
                    report.diagnostics.push("inventory limit exceeded".into());
                    break;
                }
                match entry {
                    Ok(entry) if entry.file_type().is_some_and(|f| f.is_file()) => {
                        files.push(entry.into_path())
                    }
                    Ok(_) => {}
                    Err(_) => {
                        report.complete = false;
                        report.diagnostics.push("unreadable project entry".into());
                    }
                }
            }
        }
        files.sort();
        files.dedup();
        let resolver = AttributeResolver::open(&root);
        let mut lookup = resolver.lookup();
        for path in files {
            if started.elapsed() > Duration::from_secs(30) || report.files.len() >= 10_000 {
                report.complete = false;
                report.diagnostics.push("analysis limit exceeded".into());
                break;
            }
            let canonical = path.canonicalize()?;
            if !canonical.starts_with(&root)
                || std::fs::symlink_metadata(&path)?.file_type().is_symlink()
            {
                return Err(AstroError::new(
                    ErrorCode::PathOutOfBounds,
                    "project path escapes its root",
                )
                .into());
            }
            let relative = canonical
                .strip_prefix(&root)?
                .to_string_lossy()
                .replace('\\', "/");
            let source_path = canonical.to_str().ok_or_else(|| {
                AstroError::new(ErrorCode::InvalidRequest, "source path is not UTF-8")
            })?;
            let mut header = [0u8; 4096];
            let len = std::io::Read::read(&mut std::fs::File::open(&canonical)?, &mut header)?;
            if crate::language::LangId::detect(camino::Utf8Path::new(source_path), &header[..len])
                .is_err()
            {
                report.skipped_unsupported += 1;
                continue;
            }
            if std::fs::metadata(&canonical)?.len() > 1024 * 1024 {
                report.complete = false;
                report
                    .diagnostics
                    .push(format!("oversized source: {relative}"));
                continue;
            }
            match generated::judge(lookup.linguist_generated(&canonical), &canonical, || {
                generated::is_auto_generated(&canonical)
            }) {
                GeneratedVerdict::Generated(_) => {
                    report.skipped_generated += 1;
                    continue;
                }
                GeneratedVerdict::Undetermined(_) => {
                    report.complete = false;
                    report
                        .diagnostics
                        .push(format!("attributes unavailable: {relative}"));
                    continue;
                }
                GeneratedVerdict::NotGenerated => {}
            }
            let path = canonical.to_str().ok_or_else(|| {
                AstroError::new(ErrorCode::InvalidRequest, "source path is not UTF-8")
            })?;
            match self.extract_symbols(path) {
                Ok(response) => {
                    if !response.diagnostics.is_empty() {
                        report.complete = false;
                        report
                            .diagnostics
                            .push(format!("parse diagnostics: {relative}"));
                    }
                    report.files.push(ScannedFile {
                        path: relative,
                        symbols: response.symbols.unwrap_or_default(),
                    });
                }
                Err(error)
                    if error
                        .downcast_ref::<AstroError>()
                        .is_some_and(|e| e.code == ErrorCode::UnsupportedLanguage) =>
                {
                    report.skipped_unsupported += 1
                }
                Err(_) => {
                    report.complete = false;
                    report
                        .diagnostics
                        .push(format!("analysis failed: {relative}"));
                }
            }
        }
        report.files.sort_by(|a, b| a.path.cmp(&b.path));
        report.removed.sort();
        Ok(report)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn splits_acronyms_camel_case_digits_and_unicode() {
        for (input, want) in [
            ("SessionHandler", vec!["Session", "Handler"]),
            ("HTTPServer_v2", vec!["HTTP", "Server", "v", "2"]),
            ("hello_world", vec!["hello", "world"]),
            ("日本語_入力", vec!["日本語", "入力"]),
        ] {
            assert_eq!(split_identifier(input), want);
        }
    }
    #[test]
    fn incremental_scan_does_not_read_other_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn alpha() {}\n").unwrap();
        std::fs::write(dir.path().join("b.rs"), "fn beta() {}\n").unwrap();
        let service = AppService::sandboxed(dir.path().into()).unwrap();
        let report = service
            .scan_symbols(dir.path().to_str().unwrap(), Some(&["a.rs".into()]))
            .unwrap();
        assert!(report.complete);
        assert_eq!(report.files.len(), 1);
        assert_eq!(report.files[0].symbols[0].name, "alpha");
        assert!(
            service
                .scan_symbols(
                    dir.path().to_str().unwrap(),
                    Some(&["../outside.rs".into()])
                )
                .is_err()
        );
    }
    #[test]
    fn generated_sources_are_reported_and_skipped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("gen.rs"),
            "// This file is @generated\nfn generated() {}\n",
        )
        .unwrap();
        let report = AppService::new()
            .scan_symbols(dir.path().to_str().unwrap(), Some(&["gen.rs".into()]))
            .unwrap();
        assert_eq!(report.skipped_generated, 1);
        assert!(report.files.is_empty());
    }
}
