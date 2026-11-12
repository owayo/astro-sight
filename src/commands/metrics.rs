use std::path::Path;

use anyhow::Result;

use crate::models::metrics::{MetricsOptions, MetricsResult};
use crate::service::AppService;

pub(crate) fn attach_baseline(
    dir: &Path,
    opts: &MetricsOptions,
    result: &mut MetricsResult,
) -> Result<()> {
    let Some(base) = &opts.base else {
        return Ok(());
    };
    let (snapshot, commit, omissions) = super::git_snapshot::prepare_commit_snapshot(dir, base)?;
    let baseline_options = MetricsOptions {
        dir: snapshot.dir()?.to_string(),
        base: None,
        ..opts.clone()
    };
    let sandbox = AppService::sandboxed(Path::new(snapshot.dir()?).to_path_buf())?;
    let mut baseline = sandbox.analyze_metrics(&baseline_options)?;
    if !omissions.is_empty() {
        baseline.resolution_summary.all_static_imports_resolved = false;
        baseline.diagnostics.extend(omissions);
        baseline.diagnostics.sort_by(|a, b| {
            a.path
                .cmp(&b.path)
                .then(a.line.cmp(&b.line))
                .then(a.reason.cmp(&b.reason))
                .then(a.message.cmp(&b.message))
        });
    }
    crate::engine::metrics::comparison::attach(result, baseline, commit)
}
