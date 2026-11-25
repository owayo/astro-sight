//! Clap 4 の Parser derive が Command::about に渡す文字列だけを比較から外す。
use super::*;
use crate::commands::api_changes::rust_dependencies::crates::Located;
use crate::commands::git_input::GitBlobBatch;
use crate::engine::bounded_read::read_input_text_file;
use crate::engine::impact::rust_cargo_config::{CargoConfigProof, config_keeps_sources};
use crate::engine::rust_edition::RustEdition;
use crate::git_support::{TreeView, normalize_workspace_separators, tree_view};
use std::path::{Path, PathBuf};
use tree_sitter::Node;

pub(super) fn about<'a>(attributes: &[Node<'a>], source: &[u8]) -> Option<Node<'a>> {
    let [derive, command] = attributes else {
        return None;
    };
    let mut tokens = Vec::new();
    push_tokens(*derive, source, &mut tokens);
    if tokens
        != [
            "#",
            "[",
            "identifier:derive",
            "(",
            "identifier:clap",
            "::",
            "identifier:Parser",
            ")",
            "]",
        ]
    {
        return None;
    }
    tokens.clear();
    push_tokens(*command, source, &mut tokens);
    let [_, _, _, _, _, _, literal, _, _] = tokens.as_slice() else {
        return None;
    };
    (tokens[..6] == ["#", "[", "identifier:command", "(", "identifier:about", "="]
        && tokens[7..] == [")", "]"]
        && (literal.starts_with("string_literal:\"")
            || literal.starts_with("raw_string_literal:r")))
    .then_some(*command)
}

pub(super) fn compare(
    old: &Snapshot,
    new: &mut Snapshot,
    library: Option<&LibraryVersions<'_>>,
    inputs: &DetectionInputs<'_>,
    package: &str,
) {
    let Some(library) = library else { return };
    let trusted = std::cell::OnceCell::new();
    for (key, (module, normalized)) in &new.clap_help {
        if old
            .clap_help
            .get(key)
            .is_none_or(|(place, hash)| place != module || hash != normalized)
        {
            continue;
        }
        let Some([before]) = old.declarations.get(key).map(Vec::as_slice) else {
            continue;
        };
        let Some([after]) = new.declarations.get(key).map(Vec::as_slice) else {
            continue;
        };
        if before.fingerprint != after.fingerprint
            && old.confines(&key.0, module)
            && new.confines(&key.0, module)
            && (module.is_empty()
                || [&library.old, &library.new].iter().all(|version| {
                    matches!(version.locate_scope(&[]),
                    Located::Found(root, scope)
                    if root.known_derive_scope(&scope, 0))
                }))
            && *trusted.get_or_init(|| {
                old.confinable(Some(library.old.placements()))
                    && new.confinable(Some(library.new.placements()))
                    && registry_clap(inputs, package, library) == Some(())
            })
        {
            new.declarations.get_mut(key).unwrap()[0].fingerprint = before.fingerprint;
        }
    }
}

// 旧 tree と新しい解析対象のファイルの不在を、読み取り失敗や snapshot が省いたリンクと区別する。
enum Files<'a> {
    Base(&'a GitBlobBatch),
    Current(&'a Path, &'a TreeView),
}

impl Files<'_> {
    fn directory(&self, path: &Path) -> bool {
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

    fn text(&self, path: &Path) -> Option<Option<String>> {
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

/// 既定の crates.io から Clap 4 を選ぶ通常依存で、derive feature を使う。読み替え・別名は扱わない。
fn clap_spec(spec: &toml::Value) -> bool {
    let Some(table) = spec.as_table() else {
        return false;
    };
    let Some(version) = table.get("version").and_then(toml::Value::as_str) else {
        return false;
    };
    let version = version.trim();
    let version = version.strip_prefix(['^', '=', '~']).unwrap_or(version);
    let mut parts = version.split('.');
    parts.next() == Some("4")
        && parts.clone().count() <= 2
        && parts.all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        && table.keys().all(|key| {
            matches!(
                key.as_str(),
                "version" | "features" | "default-features" | "optional"
            )
        })
        && table
            .get("optional")
            .is_none_or(|v| v.as_bool() == Some(false))
        && table
            .get("features")
            .and_then(toml::Value::as_array)
            .is_some_and(|features| features.iter().any(|f| f.as_str() == Some("derive")))
}

struct CargoProof {
    manifests: blake3::Hash,
    lock: Option<blake3::Hash>,
}

fn cargo_fingerprint(text: &str) -> blake3::Hash {
    let mut hash = blake3::Hasher::new();
    for line in text.lines() {
        hash.update(line.as_bytes());
        hash.update(b"\n");
    }
    hash.finalize()
}

fn uses_clap(files: &Files<'_>, package: &Path) -> Option<CargoProof> {
    let mut workspace = None;
    let mut package_name = None;
    let mut manifests = blake3::Hasher::new();
    for directory in package.ancestors() {
        if !directory.as_os_str().is_empty() && !files.directory(directory) {
            return None;
        }
        let config = directory.join(".cargo");
        if !files.directory(&config) {
            return None;
        }
        for name in ["config", "config.toml"] {
            if files
                .text(&config.join(name))?
                .is_some_and(|text| !config_keeps_sources(&text))
            {
                return None;
            }
        }
        let Some(source) = files.text(&directory.join("Cargo.toml"))? else {
            if directory == package {
                return None;
            }
            continue;
        };
        manifests.update(cargo_fingerprint(&source).as_bytes());
        let manifest = toml::from_str::<toml::Table>(&source).ok()?;
        if manifest.contains_key("patch") || manifest.contains_key("replace") {
            return None;
        }
        if manifest.contains_key("workspace") {
            if workspace.is_some() {
                return None;
            }
            workspace = Some(directory);
        }
        if directory != package {
            continue;
        }
        let package_table = manifest.get("package")?.as_table()?;
        package_name = Some(package_table.get("name")?.as_str()?.to_owned());
        if package_table.contains_key("workspace")
            || !clap_spec(manifest.get("dependencies")?.get("clap")?)
        {
            return None;
        }
        if let Some(targets) = manifest.get("target") {
            for target in targets.as_table()?.values() {
                if target
                    .get("dependencies")
                    .is_some_and(|d| d.get("clap").is_some())
                {
                    return None;
                }
            }
        }
    }
    let workspace = workspace.unwrap_or(package);
    // workspace から exclude された package は自分の Cargo.lock を使う。
    if workspace != package && files.text(&package.join("Cargo.lock"))?.is_some() {
        return None;
    }
    let lock = workspace.join("Cargo.lock");
    let source = files.text(&lock)?;
    if let Some(source) = &source {
        let lock = toml::from_str::<toml::Table>(source).ok()?;
        let entries = lock.get("package")?.as_array()?;
        let unique = |name: &str| {
            let mut found = entries
                .iter()
                .filter(|entry| entry.get("name").and_then(toml::Value::as_str) == Some(name));
            let entry = found.next()?;
            found.next().is_none().then_some(entry)
        };
        let depends_on = |entry: &toml::Value, name: &str| {
            entry
                .get("dependencies")
                .and_then(toml::Value::as_array)
                .is_some_and(|deps| deps.iter().any(|dep| dep.as_str() == Some(name)))
        };
        let owner = unique(package_name.as_deref()?)?;
        let clap = unique("clap")?;
        if owner.get("source").is_some()
            || !depends_on(owner, "clap")
            || !depends_on(clap, "clap_derive")
            || !depends_on(clap, "clap_builder")
        {
            return None;
        }
        for entry in [clap, unique("clap_builder")?, unique("clap_derive")?] {
            if !entry.get("version")?.as_str()?.starts_with("4.")
                || entry.get("source")?.as_str()?
                    != "registry+https://github.com/rust-lang/crates.io-index"
            {
                return None;
            }
        }
    }
    Some(CargoProof {
        manifests: manifests.finalize(),
        lock: source.as_deref().map(cargo_fingerprint),
    })
}

fn registry_clap(
    inputs: &DetectionInputs<'_>,
    package: &str,
    library: &LibraryVersions<'_>,
) -> Option<()> {
    if library.old.edition(package)? != RustEdition::E2018OrLater
        || library.new.edition(package)? != RustEdition::E2018OrLater
    {
        return None;
    }
    // crate root の macro_use は子にも効く。ファイルの配置が完全な場合だけ呼び出す。
    for version in [&library.old, &library.new] {
        if !version.placements().files.values().flatten().all(|place| {
            place
                .index
                .as_ref()
                .is_some_and(|file| file.keeps_macro_prelude())
        }) {
            return None;
        }
    }
    let root = Path::new(inputs.dir);
    let checkout = std::path::absolute(inputs.checkout).ok()?;
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
    let package = PathBuf::from(package);
    let old = uses_clap(&Files::Base(inputs.base_blobs), &package)?;
    let new = uses_clap(&Files::Current(root, &tree_view(root)), &package)?;
    (old.manifests == new.manifests && old.lock.zip(new.lock).is_none_or(|(old, new)| old == new))
        .then_some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clap_about_checks_an_excluded_packages_own_lock() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let package = Path::new("member");
        std::fs::create_dir(root.join(package)).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\nexclude=['member']").unwrap();
        std::fs::write(root.join("member/Cargo.toml"), "[package]\nname='app'\nversion='1.0.0'\n[dependencies]\nclap={version='4',features=['derive']}").unwrap();
        let view = tree_view(root);
        let files = Files::Current(root, &view);
        assert!(uses_clap(&files, package).is_some());
        std::fs::write(root.join("member/Cargo.lock"), "invalid").unwrap();
        assert!(uses_clap(&files, package).is_none());
    }
}
