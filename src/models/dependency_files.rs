//! 依存宣言ファイル (manifest) とロックファイルの正本テーブル。
//!
//! 同じ集合を 2 箇所で持つと片方だけ更新されて「言語によって挙動が違う」形で静かに
//! 壊れる。実際に `BLAME_DEFAULT_EXCLUDE_GLOBS` (cochange エンジンの候補除外) と
//! review 側の manifest/lock ペア表が独立に存在し、後者にしかない
//! `uv.lock` / `poetry.lock` / `pdm.lock` / `Gemfile.lock` / `go.sum` / `mix.lock` の 6 個が
//! 共変更候補に残っていた (Rust / npm プロジェクトでは同じ差分でも lock の誤検出が出ず、
//! Python / Ruby / Go / Elixir だけ出るという非一貫性)。
//!
//! このモジュールを唯一の正本とし、glob 文字列ではなく「ファイルの意味」で判定する。
//! ペアを 1 行足せば候補除外もペア判定も同時に追随する。

/// 依存宣言ファイルと、それに対応するロックファイルの組。
#[derive(Debug, Clone, Copy)]
pub struct DependencyManifestLock {
    /// 人が編集する依存宣言ファイル (`Cargo.toml` / `pyproject.toml` 等)。
    pub manifest: &'static str,
    /// パッケージマネージャが生成するロックファイル (`Cargo.lock` / `uv.lock` 等)。
    pub lock: &'static str,
}

/// 依存マニフェストとロックファイルの既知ペア。
///
/// `cargo update` や `npm install` のように片側のみが変更される正規操作が頻繁に発生するため、
/// 共変更の警告対象としては扱わない。
pub const DEPENDENCY_MANIFEST_LOCK_PAIRS: &[DependencyManifestLock] = &[
    DependencyManifestLock {
        manifest: "Cargo.toml",
        lock: "Cargo.lock",
    },
    DependencyManifestLock {
        manifest: "package.json",
        lock: "package-lock.json",
    },
    DependencyManifestLock {
        manifest: "package.json",
        lock: "pnpm-lock.yaml",
    },
    DependencyManifestLock {
        manifest: "package.json",
        lock: "yarn.lock",
    },
    DependencyManifestLock {
        manifest: "pyproject.toml",
        lock: "uv.lock",
    },
    DependencyManifestLock {
        manifest: "pyproject.toml",
        lock: "poetry.lock",
    },
    DependencyManifestLock {
        manifest: "pyproject.toml",
        lock: "pdm.lock",
    },
    DependencyManifestLock {
        manifest: "Gemfile",
        lock: "Gemfile.lock",
    },
    DependencyManifestLock {
        manifest: "composer.json",
        lock: "composer.lock",
    },
    DependencyManifestLock {
        manifest: "go.mod",
        lock: "go.sum",
    },
    DependencyManifestLock {
        manifest: "mix.exs",
        lock: "mix.lock",
    },
];

/// パスの basename を返す。非 UTF-8 は `None`。
fn base_name(path: &str) -> Option<&str> {
    std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
}

/// パスがロックファイル (パッケージマネージャの生成物) なら true。
///
/// monorepo でどのディレクトリに置かれていても生成物であることは変わらないため、
/// ディレクトリは問わず basename だけで判定する。
pub fn is_dependency_lock_path(path: &str) -> bool {
    let Some(base) = base_name(path) else {
        return false;
    };
    DEPENDENCY_MANIFEST_LOCK_PAIRS
        .iter()
        .any(|p| p.lock == base)
}

/// パスが依存宣言ファイル (人が編集する manifest) なら true。
pub fn is_dependency_manifest_path(path: &str) -> bool {
    let Some(base) = base_name(path) else {
        return false;
    };
    DEPENDENCY_MANIFEST_LOCK_PAIRS
        .iter()
        .any(|p| p.manifest == base)
}

/// 2 つのパスが既知の依存マニフェスト/ロックペアであれば true を返す。
///
/// monorepo で別プロジェクトの manifest と lock を組にしないよう、親ディレクトリが
/// 一致する場合のみ真とする。
pub fn is_dependency_manifest_pair(file_a: &str, file_b: &str) -> bool {
    let path_a = std::path::Path::new(file_a);
    let path_b = std::path::Path::new(file_b);
    let (Some(base_a), Some(base_b)) = (
        path_a.file_name().and_then(|s| s.to_str()),
        path_b.file_name().and_then(|s| s.to_str()),
    ) else {
        return false;
    };
    if path_a.parent() != path_b.parent() {
        return false;
    }
    DEPENDENCY_MANIFEST_LOCK_PAIRS.iter().any(|p| {
        (base_a == p.manifest && base_b == p.lock) || (base_a == p.lock && base_b == p.manifest)
    })
}
