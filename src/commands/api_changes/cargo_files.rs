//! 取得元の証明で、旧 tree と新しい解析対象の Cargo の入力を同じ規則で確かめる。
use crate::commands::git_input::GitBlobBatch;
use crate::engine::bounded_read::read_input_text_file;
use crate::engine::impact::rust_cargo_config::CargoConfigProof;
use crate::git_support::{TreeView, normalize_workspace_separators};
use std::path::Path;

// 旧 tree と新しい解析対象のファイルの不在を、読み取り失敗や snapshot が省いたリンクと区別する。
pub(super) enum Files<'a> {
    Base(&'a GitBlobBatch),
    Current(&'a Path, &'a TreeView),
}

impl Files<'_> {
    pub(super) fn directory(&self, path: &Path) -> bool {
        match self {
            Self::Base(base) => matches!(
                base.tree_mode(&normalize_workspace_separators(&path.to_string_lossy())),
                Some(0 | 0o040000)
            ),
            Self::Current(root, view) => {
                !view.hides(path)
                    && match std::fs::symlink_metadata(root.join(path)) {
                        Ok(meta) => meta.file_type().is_dir(),
                        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
                    }
            }
        }
    }

    pub(super) fn text(&self, path: &Path) -> Option<Option<String>> {
        match self {
            Self::Base(base) => {
                let relative = normalize_workspace_separators(path.to_str()?);
                match base.tree_mode(&relative)? {
                    0 => Some(None),
                    0o100644 | 0o100755 => {
                        Some(Some(String::from_utf8(base.read(&relative)?).ok()?))
                    }
                    _ => None,
                }
            }
            Self::Current(root, view) if !view.hides(path) => {
                match std::fs::symlink_metadata(root.join(path)) {
                    Ok(meta) if meta.is_file() => read_input_text_file(&root.join(path)).map(Some),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(None),
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

pub(super) fn external_settings_known(checkout: &Path) -> Option<()> {
    let checkout = std::path::absolute(checkout).ok()?;
    let config = CargoConfigProof::capture(&checkout);
    // snapshot の外の設定は実際の checkout の祖先と CARGO_HOME で確かめる。
    for checkout in [&checkout, &std::fs::canonicalize(&checkout).ok()?] {
        let parent = checkout.parent()?;
        if !config.keeps_dependency_sources(parent)
            || parent.ancestors().any(|dir| {
                match std::fs::symlink_metadata(dir.join("Cargo.toml")) {
                    Err(error) => error.kind() != std::io::ErrorKind::NotFound,
                    Ok(_) => true,
                }
            })
        {
            return None;
        }
    }
    Some(())
}
