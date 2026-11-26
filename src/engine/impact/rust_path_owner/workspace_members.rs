use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use glob::Pattern;

use super::{lexical_join, under_root_git_or_target};

const METACHARS: [char; 3] = ['*', '?', '['];

/// どの workspace も、走査しない root 直下の target・.git から package を選ばないか。
pub(super) fn outside_unscanned(
    root: &Path,
    directory: &Path,
    manifest: &toml::Value,
) -> Option<()> {
    let Some(workspace) = manifest.get("workspace") else {
        return Some(());
    };
    let Some(members) = workspace.as_table()?.get("members") else {
        return Some(());
    };
    let members: Vec<_> = members
        .as_array()?
        .iter()
        .map(toml::Value::as_str)
        .collect::<Option<_>>()?;
    // Cargo の exclude は workspace からのリテラルの接頭辞。明記された member の方が優先する。
    // ".." を含む exclude は Cargo が展開済みの絶対パスと比較すると一致しないことがある。
    let mut excluded = Vec::new();
    if let Some(exclude) = workspace.get("exclude") {
        for entry in exclude.as_array()? {
            let entry = Path::new(entry.as_str()?);
            if entry
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
            {
                excluded.push(directory.join(entry));
            }
        }
    }
    let mut explicit = Vec::new();
    for &member in &members {
        let relative = lexical_join(directory, Path::new(member))?;
        // glob の展開結果が空でも、その綴り自体が実在する package なら Cargo は採用する。
        if under_root_git_or_target(&relative) {
            let valid_filename = !cfg!(windows) || !member.contains(['*', '?']);
            if !member.contains(METACHARS)
                || valid_filename && manifest_exists(&root.join(&relative))?
            {
                return None;
            }
        }
        explicit.push(relative);
    }
    for member in members
        .into_iter()
        .filter(|member| member.contains(METACHARS))
    {
        let mut base = directory.to_owned();
        let mut pattern = Vec::new();
        for part in Path::new(member).components() {
            match part {
                Component::CurDir => {}
                Component::ParentDir if pattern.is_empty() && base.pop() => {}
                Component::Normal(name) => {
                    let name = name.to_str()?;
                    if pattern.is_empty() && !name.contains(METACHARS) {
                        base.push(name);
                    } else {
                        pattern.push(Pattern::new(name).ok()?);
                    }
                }
                _ => return None,
            }
        }
        if (base.as_os_str().is_empty() || under_root_git_or_target(&base))
            && glob_selects_package(root, &base, &pattern, &explicit, &excluded)?
        {
            return None;
        }
    }
    Some(())
}

/// Cargo と同じ glob を要素ごとに照合する。リテラルの要素は FS で引き、"**" は深さを限定しない。
/// root の直下は走査対象外の 2 つに限る。読めない部分・リンクの循環・探索上限は証明に使わない。
fn glob_selects_package(
    root: &Path,
    base: &Path,
    pattern: &[Pattern],
    explicit: &[PathBuf],
    excluded: &[PathBuf],
) -> Option<bool> {
    let view = crate::git_support::tree_view(root);
    let omitted_manifest = view.omitted()?.iter().any(|(path, _)| {
        under_root_git_or_target(path)
            && path
                .file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case("Cargo.toml"))
    });
    let mut pending = vec![(base.to_owned(), 0)];
    let mut visited = HashSet::new();
    let mut budget = 10_000usize;
    while let Some((relative, index)) = pending.pop() {
        if !visited.insert((relative.clone(), index)) {
            continue;
        }
        // snapshot で省かれた manifest の親は、ディレクトリごと書き出されていないこともある。
        if omitted_manifest && !relative.as_os_str().is_empty() {
            return None;
        }
        let path = root.join(&relative);
        let Some(part) = pattern.get(index) else {
            if !relative.as_os_str().is_empty()
                && (explicit.iter().any(|member| relative.starts_with(member))
                    || !excluded.iter().any(|exclude| relative.starts_with(exclude)))
                && manifest_exists(&path)?
            {
                return Some(true);
            }
            continue;
        };
        let text = part.as_str();
        if !text.contains(METACHARS) {
            let next = relative.join(text);
            if !relative.as_os_str().is_empty() || under_root_git_or_target(&next) {
                pending.push((next, index + 1));
            }
            continue;
        }
        let recursive = text == "**";
        if recursive {
            pending.push((relative.clone(), index + 1));
        }
        let next_index = if recursive { index } else { index + 1 };
        if relative.as_os_str().is_empty() {
            for top in ["target", ".git"] {
                if recursive || part.matches(top) {
                    pending.push((PathBuf::from(top), next_index));
                }
            }
            continue;
        }
        let entries = match std::fs::read_dir(&path) {
            Ok(entries) => entries,
            Err(err) if matches!(err.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
                continue;
            }
            Err(_) => return None,
        };
        for entry in entries {
            budget = budget.checked_sub(1)?;
            let entry = entry.ok()?;
            let name = entry.file_name();
            if !recursive && !part.matches(name.to_str()?) {
                continue;
            }
            let kind = entry.file_type().ok()?;
            if kind.is_symlink() {
                return None;
            }
            if kind.is_dir() {
                pending.push((relative.join(name), next_index));
            }
        }
    }
    Some(false)
}

fn manifest_exists(directory: &Path) -> Option<bool> {
    match std::fs::metadata(directory.join("Cargo.toml")) {
        Ok(_) => Some(true),
        Err(err) if matches!(err.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
            Some(false)
        }
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_member_with_literal_brackets_can_name_a_package() {
        let repo = tempfile::tempdir().unwrap();
        let package = repo.path().join("target/pr[o]be");
        std::fs::create_dir_all(&package).unwrap();
        let manifest =
            toml::from_str("[workspace]\nmembers=['target/pr[o]be']\nexclude=['target']").unwrap();
        assert!(outside_unscanned(repo.path(), Path::new(""), &manifest).is_some());
        std::fs::write(package.join("Cargo.toml"), "[package]\nname='probe'").unwrap();
        assert!(outside_unscanned(repo.path(), Path::new(""), &manifest).is_none());
    }
}
