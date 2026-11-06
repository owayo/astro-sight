//! TS / JS のモジュールの指定子とパスを扱う低層の部品。props 削除などの証明が使う信頼の境界の
//! 監査 (`ts_signature::repository_audit`) と、削除の参照の帰属で別名を解く
//! (`ts_alias_resolution`) が共有する。どちらの判定の規則もここには置かない。

use std::path::{Component, Path, PathBuf};

/// 解決で補う TS / JS の拡張子 (宣言ファイルを含む)。
pub(crate) const RESOLVED_EXTENSIONS: [&str; 11] = [
    ".ts", ".tsx", ".d.ts", ".mts", ".cts", ".d.mts", ".d.cts", ".js", ".jsx", ".mjs", ".cjs",
];

pub(crate) fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// ファイルとして解くときの候補 (拡張子の補完と、`.js` → `.ts` などの読み替え)。
pub(crate) fn file_candidates(joined: &Path) -> Vec<PathBuf> {
    let mut candidates = vec![joined.to_path_buf()];
    let Some(name) = joined.file_name().and_then(|name| name.to_str()) else {
        return candidates;
    };
    let stem_with = |stem: &str, extensions: &[&str]| -> Vec<PathBuf> {
        extensions
            .iter()
            .map(|extension| joined.with_file_name(format!("{stem}{extension}")))
            .collect()
    };
    match name.rsplit_once('.') {
        Some((stem, "js" | "jsx")) => candidates.extend(stem_with(stem, &[".ts", ".tsx", ".d.ts"])),
        Some((stem, "mjs")) => candidates.extend(stem_with(stem, &[".mts", ".d.mts"])),
        Some((stem, "cjs")) => candidates.extend(stem_with(stem, &[".cts", ".d.cts"])),
        Some((_, "ts" | "tsx" | "mts" | "cts")) => {}
        _ => candidates.extend(
            RESOLVED_EXTENSIONS
                .iter()
                .map(|extension| with_suffix(joined, extension)),
        ),
    }
    candidates
}

/// `.` と `..` を字句の上で畳む。根より上へ出るなら None。symlink は解決しないので、途中に
/// symlink があるパスでは実際の位置と一致しない (使う側で symlink を除く)。
pub(crate) fn normalize_lexically(path: &Path) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            Component::Normal(name) => normalized.push(name),
        }
    }
    Some(normalized)
}

/// `/` 区切りの 2 つのパスが、同じファイルを指し得るか。大文字小文字を区別しないファイルシステム
/// (Windows と macOS の既定) では綴りの違うパスも同じファイルになり、macOS は Unicode の正規化も
/// 区別しない。区切りごとに [`segment_skeleton`] で比べる。
pub(crate) fn may_be_same_path(a: &str, b: &str) -> bool {
    let (a, b): (Vec<&str>, Vec<&str>) = (a.split('/').collect(), b.split('/').collect());
    a.len() == b.len() && segments_may_match(&a, &b)
}

/// `/` 区切りのパス `path` が、ディレクトリ `directory` の下にあり得るか ([`may_be_same_path`] と
/// 同じ比べ方)。
pub(crate) fn may_be_under(path: &str, directory: &str) -> bool {
    let (path, directory): (Vec<&str>, Vec<&str>) =
        (path.split('/').collect(), directory.split('/').collect());
    path.len() > directory.len() && segments_may_match(&path[..directory.len()], &directory)
}

fn segments_may_match(a: &[&str], b: &[&str]) -> bool {
    a.iter()
        .zip(b)
        .all(|(a, b)| segment_skeleton(a) == segment_skeleton(b))
}

/// 区切り 1 つの、大文字小文字と正規化の違いを畳んだ形。ASCII は小文字にし、ASCII でない文字の
/// 並びは 1 つの印 (`\0`) にまとめる (同じ印どうしは同じ文字かもしれないとみなす)。ASCII の英数字の
/// 直後の結合文字は、その英数字ごと印にする (NFD の `e` + U+0301 と NFC の `é` を同じにする)。
/// 大文字小文字の畳み込みで ASCII になる文字 (KELVIN SIGN・LONG S・ß・合字) は先に展開する。
fn segment_skeleton(segment: &str) -> String {
    let mut skeleton = String::with_capacity(segment.len());
    for character in segment.chars() {
        let folded = match character {
            '\u{212A}' => "k",
            '\u{017F}' => "s",
            '\u{00DF}' | '\u{1E9E}' => "ss",
            '\u{FB00}' => "ff",
            '\u{FB01}' => "fi",
            '\u{FB02}' => "fl",
            '\u{FB03}' => "ffi",
            '\u{FB04}' => "ffl",
            '\u{FB05}' | '\u{FB06}' => "st",
            _ => "",
        };
        if !folded.is_empty() {
            skeleton.push_str(folded);
            continue;
        }
        if character.is_ascii() {
            skeleton.push(character.to_ascii_lowercase());
            continue;
        }
        if is_combining_mark(character) && skeleton.ends_with(|c: char| c.is_ascii_alphanumeric()) {
            skeleton.pop();
        }
        if !skeleton.ends_with('\0') {
            skeleton.push('\0');
        }
    }
    skeleton
}

fn is_combining_mark(character: char) -> bool {
    use unicode_general_category::{GeneralCategory, get_general_category};
    matches!(
        get_general_category(character),
        GeneralCategory::NonspacingMark
            | GeneralCategory::SpacingMark
            | GeneralCategory::EnclosingMark
    )
}

/// bare specifier のパッケージ名 (`@scope/name` か先頭の区切り)。相対・絶対・`#` の subpath
/// import・URL のように、パッケージを指さない指定子は None。
pub(crate) fn package_name(specifier: &str) -> Option<&str> {
    if specifier.is_empty()
        || specifier.starts_with(['.', '/', '#', '~'].as_slice())
        || specifier.contains(['\\', ' ', '\t', '\n'].as_slice())
    {
        return None;
    }
    if let Some(builtin) = specifier.strip_prefix("node:") {
        return (!builtin.is_empty()).then(|| specifier.split('/').next().unwrap_or(specifier));
    }
    if specifier.contains(':') {
        return None;
    }
    let mut segments = specifier.split('/');
    let first = segments.next()?;
    if first.starts_with('@') {
        let second = segments.next()?;
        if first.len() < 2 || second.is_empty() {
            return None;
        }
        return Some(&specifier[..first.len() + 1 + second.len()]);
    }
    (!first.is_empty()).then_some(first)
}

/// 依存の版の指定が registry のものか。`file:`・`link:`・`workspace:`・`portal:`・パス・git や
/// URL の指定は registry ではない。`npm:` の別名は registry の別のパッケージを指す。
pub(crate) fn registry_spec(spec: &str) -> bool {
    const NON_REGISTRY: [&str; 13] = [
        "file:",
        "link:",
        "workspace:",
        "portal:",
        "git+",
        "git:",
        "git@",
        "github:",
        "gitlab:",
        "bitbucket:",
        "gist:",
        "http:",
        "https:",
    ];
    let spec = spec.trim();
    if spec.starts_with("npm:") {
        return true;
    }
    !(NON_REGISTRY.iter().any(|prefix| spec.starts_with(prefix))
        || spec.starts_with(['.', '/', '~'].as_slice())
        || spec.contains(['\\', '/', ':'].as_slice()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 大文字小文字だけが違うパス、ASCII でない文字の大文字小文字か正規化 (NFC / NFD) だけが違う
    /// パス、ASCII へ畳み込まれる文字を使ったパスは、同じファイルを指し得る。区切りの数・ASCII の
    /// 綴りが違うパスは別のファイル。
    #[test]
    fn paths_that_may_name_the_same_file() {
        assert!(may_be_same_path(
            "src/Server/errors.ts",
            "src/server/Errors.ts"
        ));
        assert!(may_be_same_path("src/Äpi.ts", "src/äpi.ts"));
        assert!(may_be_same_path("src/caf\u{e9}.ts", "src/cafe\u{301}.ts"));
        assert!(may_be_same_path("src/\u{212A}ey.ts", "src/key.ts"));
        assert!(may_be_same_path("src/stra\u{df}e.ts", "src/STRASSE.ts"));
        assert!(may_be_same_path("src/旧.ts", "src/新.ts"));
        assert!(!may_be_same_path("src/旧.ts", "src/transport.ts"));
        assert!(!may_be_same_path("src/api.ts", "src/apis.ts"));
        assert!(!may_be_same_path("src/日本/x.ts", "src/x.ts"));
        assert!(may_be_under("types/Lodash/index.d.ts", "types/lodash"));
        assert!(!may_be_under("types/lodash", "types/lodash"));
        assert!(!may_be_under("types/lodashx/index.d.ts", "types/lodash"));
    }
}
