//! Cargo の manifest から、library target の Rust edition を決める。
//!
//! use path の解決規則は edition で変わる。2015 は先頭が識別子の path をクレートルートから
//! 読み、2018 以降は現在の module のスコープと extern prelude から読む。読み方を間違えると
//! `pub use` の再公開を辿れなくなるので、決められないときは `None` (不明) を返し、
//! 呼び出し側に推測させない。

use std::path::{Component, Path, PathBuf};

/// use path の解決規則に効く edition の区別。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RustEdition {
    /// 2015: 先頭が識別子の use path をクレートルートから読む。
    E2015,
    /// 2018 / 2021 / 2024: 先頭の名前を現在の module のスコープ、次に extern prelude から読む。
    E2018OrLater,
}

/// manifest はあるが読めない (読み込みの失敗・上限超過・通常のファイルでない)。
/// 不在 (`Ok(None)`) とは区別する。不在なら祖先の探索を続けられるが、読めない manifest は
/// workspace の root かどうか分からないので、そこで判定を諦める。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ManifestUnreadable;

/// `package_dir` (解析ルートからの相対パス) の package の library target の edition。
///
/// 決める順序は `[lib].edition` → `[package].edition` → (`edition.workspace = true` なら)
/// 所属する workspace の `[workspace.package].edition` → `[package].edition` の省略は 2015。
/// manifest が無い・読めない・壊れている、未知の値・不正な型、継承先が解析ルートの中に
/// 見つからないときは `None`。
///
/// `read` は解析ルートからの相対パスの manifest を読み、`Ok(None)` は不在を表す。
pub(crate) fn library_edition(
    package_dir: &Path,
    mut read: impl FnMut(&Path) -> Result<Option<Vec<u8>>, ManifestUnreadable>,
) -> Option<RustEdition> {
    let manifest = parse_manifest(&read(&package_dir.join("Cargo.toml")).ok()??)?;
    // `[lib]` が表でなければ manifest として壊れている (`?` で不明にする)。
    if let Some(lib) = manifest.get("lib")
        && let Some(edition) = lib.as_table()?.get("edition")
    {
        return parse_edition(edition.as_str()?);
    }
    let package = manifest.get("package")?.as_table()?;
    let Some(edition) = package.get("edition") else {
        return Some(RustEdition::E2015);
    };
    if let Some(edition) = edition.as_str() {
        return parse_edition(edition);
    }
    // `edition.workspace = true` 以外の形 (`false` や余計なキー) は Cargo が受け付けない。
    let inherit = edition.as_table()?;
    if inherit.len() != 1 || inherit.get("workspace")?.as_bool() != Some(true) {
        return None;
    }
    let workspace = workspace_root_manifest(package_dir, &manifest, package, &mut read)?;
    parse_edition(
        workspace
            .get("workspace")?
            .get("package")?
            .get("edition")?
            .as_str()?,
    )
}

/// package が所属する workspace の root の manifest。`package.workspace` の明示を優先し、
/// 無ければ自身、次に祖先 (解析ルートまで) の `[workspace]` を持つ manifest を探す。
fn workspace_root_manifest(
    package_dir: &Path,
    manifest: &toml::Table,
    package: &toml::Table,
    read: &mut impl FnMut(&Path) -> Result<Option<Vec<u8>>, ManifestUnreadable>,
) -> Option<toml::Table> {
    if let Some(root) = package.get("workspace") {
        let root_dir = join_within_root(package_dir, Path::new(root.as_str()?))?;
        let root = parse_manifest(&read(&root_dir.join("Cargo.toml")).ok()??)?;
        root.get("workspace")?;
        return Some(root);
    }
    if manifest.contains_key("workspace") {
        return Some(manifest.clone());
    }
    let mut ancestor = package_dir.parent();
    while let Some(dir) = ancestor {
        match read(&dir.join("Cargo.toml")) {
            Ok(None) => {}
            Ok(Some(bytes)) => {
                // 壊れた祖先の manifest は workspace の root かどうか分からない。
                let candidate = parse_manifest(&bytes)?;
                if candidate.contains_key("workspace") {
                    return Some(candidate);
                }
            }
            Err(ManifestUnreadable) => return None,
        }
        ancestor = dir.parent();
    }
    // 解析ルートの外の workspace は読まない。
    None
}

/// `base` (解析ルートからの相対) に相対パス `relative` を足す。解析ルートの外へ出る・
/// 絶対パスなど、解析ルートからの相対で表せないときは `None`。
fn join_within_root(base: &Path, relative: &Path) -> Option<PathBuf> {
    let mut joined: Vec<&std::ffi::OsStr> = Vec::new();
    for component in base.components().chain(relative.components()) {
        match component {
            Component::Normal(part) => joined.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                joined.pop()?;
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(joined.into_iter().collect())
}

fn parse_manifest(bytes: &[u8]) -> Option<toml::Table> {
    std::str::from_utf8(bytes).ok()?.parse::<toml::Table>().ok()
}

fn parse_edition(edition: &str) -> Option<RustEdition> {
    match edition {
        "2015" => Some(RustEdition::E2015),
        "2018" | "2021" | "2024" => Some(RustEdition::E2018OrLater),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// 解析ルートからの相対パス → 内容。`None` は「あるが読めない」manifest。
    fn edition_of(package_dir: &str, files: &[(&str, Option<&str>)]) -> Option<RustEdition> {
        let files: HashMap<PathBuf, Option<&str>> = files
            .iter()
            .map(|(path, content)| (PathBuf::from(path), *content))
            .collect();
        library_edition(Path::new(package_dir), |path| match files.get(path) {
            None => Ok(None),
            Some(None) => Err(ManifestUnreadable),
            Some(Some(content)) => Ok(Some(content.as_bytes().to_vec())),
        })
    }

    const PACKAGE: &str = "[package]\nname = \"app\"\n";

    #[test]
    fn package_edition_and_its_omission() {
        for (edition, want) in [
            ("2015", Some(RustEdition::E2015)),
            ("2018", Some(RustEdition::E2018OrLater)),
            ("2021", Some(RustEdition::E2018OrLater)),
            ("2024", Some(RustEdition::E2018OrLater)),
            // 未知の値は推測しない
            ("2027", None),
        ] {
            let manifest = format!("{PACKAGE}edition = \"{edition}\"\n");
            assert_eq!(
                edition_of("", &[("Cargo.toml", Some(&manifest))]),
                want,
                "{edition}"
            );
        }
        // 省略は 2015
        assert_eq!(
            edition_of("", &[("Cargo.toml", Some(PACKAGE))]),
            Some(RustEdition::E2015)
        );
        // 不正な型
        assert_eq!(
            edition_of("", &[("Cargo.toml", Some("[package]\nedition = 2021\n"))]),
            None
        );
    }

    #[test]
    fn lib_edition_overrides_the_package() {
        let manifest = format!("{PACKAGE}edition = \"2021\"\n\n[lib]\nedition = \"2015\"\n");
        assert_eq!(
            edition_of("", &[("Cargo.toml", Some(&manifest))]),
            Some(RustEdition::E2015)
        );
        let manifest = format!("{PACKAGE}\n[lib]\nedition = \"2018\"\n");
        assert_eq!(
            edition_of("", &[("Cargo.toml", Some(&manifest))]),
            Some(RustEdition::E2018OrLater)
        );
        // `[lib]` に edition が無ければ package の値
        let manifest = format!("{PACKAGE}edition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n");
        assert_eq!(
            edition_of("", &[("Cargo.toml", Some(&manifest))]),
            Some(RustEdition::E2018OrLater)
        );
        // 不正な型は不明
        let manifest = format!("{PACKAGE}\n[lib]\nedition = {{ workspace = true }}\n");
        assert_eq!(edition_of("", &[("Cargo.toml", Some(&manifest))]), None);
    }

    #[test]
    fn missing_unreadable_or_broken_manifest_is_unknown() {
        assert_eq!(edition_of("", &[]), None);
        assert_eq!(edition_of("", &[("Cargo.toml", None)]), None);
        assert_eq!(edition_of("", &[("Cargo.toml", Some("[package\n"))]), None);
        // `[package]` の無い manifest (virtual manifest など)
        assert_eq!(
            edition_of("", &[("Cargo.toml", Some("[workspace]\n"))]),
            None
        );
    }

    #[test]
    fn workspace_inheritance() {
        let member = format!("{PACKAGE}edition.workspace = true\n");
        let root =
            "[workspace]\nmembers = [\"crates/app\"]\n\n[workspace.package]\nedition = \"2021\"\n";
        // 祖先の root から継承する
        assert_eq!(
            edition_of(
                "crates/app",
                &[
                    ("crates/app/Cargo.toml", Some(&member)),
                    ("Cargo.toml", Some(root))
                ]
            ),
            Some(RustEdition::E2018OrLater)
        );
        // 途中の `[workspace]` を持たない manifest は飛ばす
        assert_eq!(
            edition_of(
                "crates/app",
                &[
                    ("crates/app/Cargo.toml", Some(&member)),
                    ("crates/Cargo.toml", Some(PACKAGE)),
                    ("Cargo.toml", Some(root))
                ]
            ),
            Some(RustEdition::E2018OrLater)
        );
        // 自身が root
        let own = format!(
            "{PACKAGE}edition.workspace = true\n\n[workspace]\n\n[workspace.package]\nedition = \"2015\"\n"
        );
        assert_eq!(
            edition_of("", &[("Cargo.toml", Some(&own))]),
            Some(RustEdition::E2015)
        );
        // `package.workspace` の明示を優先する
        let explicit = format!("{PACKAGE}edition.workspace = true\nworkspace = \"../../ws\"\n");
        let other_root = "[workspace]\n\n[workspace.package]\nedition = \"2015\"\n";
        assert_eq!(
            edition_of(
                "crates/app",
                &[
                    ("crates/app/Cargo.toml", Some(&explicit)),
                    ("ws/Cargo.toml", Some(other_root)),
                    ("Cargo.toml", Some(root))
                ]
            ),
            Some(RustEdition::E2015)
        );
        // 明示先が解析ルートの外
        let outside = format!("{PACKAGE}edition.workspace = true\nworkspace = \"../..\"\n");
        assert_eq!(
            edition_of("app", &[("app/Cargo.toml", Some(&outside))]),
            None
        );
        // 解析ルートの中に root が無い (root は外にある)
        assert_eq!(
            edition_of("crates/app", &[("crates/app/Cargo.toml", Some(&member))]),
            None
        );
        // 祖先の manifest が読めない・壊れている
        for broken in [None, Some("[workspace\n")] {
            assert_eq!(
                edition_of(
                    "crates/app",
                    &[
                        ("crates/app/Cargo.toml", Some(&member)),
                        ("crates/Cargo.toml", broken),
                        ("Cargo.toml", Some(root))
                    ]
                ),
                None,
                "{broken:?}"
            );
        }
        // root に `[workspace.package].edition` が無い
        assert_eq!(
            edition_of(
                "crates/app",
                &[
                    ("crates/app/Cargo.toml", Some(&member)),
                    ("Cargo.toml", Some("[workspace]\n"))
                ]
            ),
            None
        );
        // `edition.workspace = false` は Cargo が受け付けない
        let not_inherited = format!("{PACKAGE}edition.workspace = false\n");
        assert_eq!(
            edition_of(
                "crates/app",
                &[
                    ("crates/app/Cargo.toml", Some(&not_inherited)),
                    ("Cargo.toml", Some(root))
                ]
            ),
            None
        );
    }

    /// `[workspace.package].edition` があっても、package が継承を指定しなければ継承しない。
    #[test]
    fn workspace_package_edition_is_not_inherited_without_the_marker() {
        let root = "[workspace]\n\n[workspace.package]\nedition = \"2021\"\n";
        assert_eq!(
            edition_of(
                "crates/app",
                &[
                    ("crates/app/Cargo.toml", Some(PACKAGE)),
                    ("Cargo.toml", Some(root))
                ]
            ),
            Some(RustEdition::E2015)
        );
        let own = format!("{PACKAGE}\n[workspace]\n\n[workspace.package]\nedition = \"2021\"\n");
        assert_eq!(
            edition_of("", &[("Cargo.toml", Some(&own))]),
            Some(RustEdition::E2015)
        );
    }
}
