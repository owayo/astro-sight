//! Cargo target declarations used by Rust API and module analysis.
//!
//! This is a manifest parser, not a Cargo resolver. A caller that uses the
//! result to suppress diagnostics must keep its own conservative fallback.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

pub(crate) struct RustTargets {
    pub(crate) has_package: bool,
    pub(crate) declares_lib: bool,
    pub(crate) roots_complete: bool,
    explicit_roots: HashSet<PathBuf>,
}

impl RustTargets {
    /// `package_dir` is relative to the analysis root. Explicit Cargo paths
    /// are relative to the package manifest and may leave that directory, but
    /// not the analysis root.
    pub(crate) fn parse(text: &str, package_dir: &Path) -> Option<Self> {
        let manifest = toml::from_str::<toml::Value>(text).ok()?;
        Self::from_manifest(&manifest, package_dir)
    }

    pub(crate) fn from_manifest(manifest: &toml::Value, package_dir: &Path) -> Option<Self> {
        let table = manifest.as_table()?;
        let package_value = table.get("package").or_else(|| table.get("project"));
        let package = package_value.and_then(toml::Value::as_table);
        let mut roots_complete = package_value.is_none_or(|_| package.is_some());
        let mut explicit_roots = HashSet::new();
        let declares_lib = table.contains_key("lib");
        if let Some(lib) = table.get("lib") {
            if let Some(lib) = lib.as_table() {
                if let Some(path) = lib.get("path") {
                    record_explicit_path(
                        path,
                        package_dir,
                        &mut explicit_roots,
                        &mut roots_complete,
                    );
                }
            } else {
                roots_complete = false;
            }
        }
        for target in ["bin", "example", "test", "bench"] {
            let Some(entries) = table.get(target) else {
                continue;
            };
            let Some(entries) = entries.as_array() else {
                roots_complete = false;
                continue;
            };
            for entry in entries {
                let Some(entry) = entry.as_table() else {
                    roots_complete = false;
                    continue;
                };
                if let Some(path) = entry.get("path") {
                    record_explicit_path(
                        path,
                        package_dir,
                        &mut explicit_roots,
                        &mut roots_complete,
                    );
                }
            }
        }
        if let Some(build) = package.and_then(|package| package.get("build")) {
            match build {
                toml::Value::String(_) => record_explicit_path(
                    build,
                    package_dir,
                    &mut explicit_roots,
                    &mut roots_complete,
                ),
                toml::Value::Boolean(_) => {}
                _ => roots_complete = false,
            }
        }
        Some(Self {
            has_package: package.is_some(),
            declares_lib,
            roots_complete,
            explicit_roots,
        })
    }

    pub(crate) fn has_explicit_roots(&self) -> bool {
        !self.explicit_roots.is_empty()
    }

    pub(crate) fn explicit_roots(&self) -> impl Iterator<Item = &Path> {
        self.explicit_roots.iter().map(PathBuf::as_path)
    }
}

fn record_explicit_path(
    value: &toml::Value,
    package_dir: &Path,
    roots: &mut HashSet<PathBuf>,
    complete: &mut bool,
) {
    if let Some(path) = value
        .as_str()
        .and_then(|raw| normalize_target_path(package_dir, raw))
    {
        roots.insert(path);
    } else {
        *complete = false;
    }
}

fn normalize_target_path(package_dir: &Path, raw: &str) -> Option<PathBuf> {
    if raw.is_empty() || Path::new(raw).is_absolute() {
        return None;
    }
    let mut result = PathBuf::new();
    for component in package_dir.join(raw).components() {
        match component {
            Component::Normal(part) => result.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                if !result.pop() {
                    return None;
                }
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::RustTargets;
    use std::path::Path;

    #[test]
    fn manifest_targets_normalize_paths_and_expose_library_presence() {
        let targets = RustTargets::parse(
            "[package]\nname = \"demo\"\nbuild = \"./scripts/build.rs\"\n\
             [lib]\npath = \"src/../core/lib.rs\"\n\
             [[bin]]\nname = \"tool\"\npath = \"tools/tool.rs\"\n",
            Path::new("crates/demo"),
        )
        .unwrap();
        assert!(targets.has_package && targets.declares_lib);
        for path in [
            "crates/demo/scripts/build.rs",
            "crates/demo/core/lib.rs",
            "crates/demo/tools/tool.rs",
        ] {
            assert!(targets.explicit_roots.contains(Path::new(path)), "{path}");
        }
        assert!(
            RustTargets::parse("[workspace]\nmembers = []\n", Path::new(""))
                .is_some_and(|targets| !targets.has_package)
        );
        let ambiguous = RustTargets::parse(
            "[package]\nname = \"demo\"\n[[bin]]\npath = \"../../../escape.rs\"\n",
            Path::new("crates/demo"),
        )
        .unwrap();
        assert!(!ambiguous.roots_complete);
        assert!(!ambiguous.declares_lib);
    }
}
