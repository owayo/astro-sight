//! 依存宣言ファイル (manifest) / ロックファイル / 対象言語の正本テーブル。
//!
//! 同じ集合を 2 箇所で持つと片方だけ更新されて「言語によって挙動が違う」形で静かに
//! 壊れる。実際に `BLAME_DEFAULT_EXCLUDE_GLOBS` (cochange エンジンの候補除外) と
//! review 側の manifest/lock ペア表が独立に存在し、後者にしかない
//! `uv.lock` / `poetry.lock` / `pdm.lock` / `Gemfile.lock` / `go.sum` / `mix.lock` の 6 個が
//! 共変更候補に残っていた (Rust / npm プロジェクトでは同じ差分でも lock の誤検出が出ず、
//! Python / Ruby / Go / Elixir だけ出るという非一貫性)。
//!
//! このモジュールを唯一の正本とし、glob 文字列ではなく「ファイルの意味」で判定する。
//! ecosystem を 1 行足せば候補除外・ペア判定・review policy が同時に追随する。

use crate::language::LangId;

/// 1 つのパッケージ管理エコシステム。
///
/// `langs` は「その manifest が依存を宣言する対象言語」。依存宣言ファイルとソースの
/// 共変更を評価するときに ecosystem をまたいだ組 (`Cargo.toml` ↔ `tools/release.py` 等) を
/// 弾くために使う。astro-sight が解析しない言語のエコシステム (Elixir 等) は空スライスで、
/// その場合ソース側の判定が成立しないため組は作られない。
#[derive(Debug, Clone, Copy)]
pub struct DependencyEcosystem {
    /// 人が編集する依存宣言ファイル (`Cargo.toml` / `pyproject.toml` 等)。
    pub manifest: &'static str,
    /// パッケージマネージャが生成するロックファイル。同一 manifest に複数あり得る
    /// (`package.json` に対する npm / pnpm / yarn)。
    pub locks: &'static [&'static str],
    /// この manifest が依存を宣言する対象言語。
    pub langs: &'static [LangId],
}

/// 依存マニフェスト / ロックファイル / 対象言語の既知エコシステム。
///
/// lock は `cargo update` や `npm install` のように片側のみが変更される正規操作が頻繁に
/// 発生するため、共変更の警告対象としては扱わない。
pub const DEPENDENCY_ECOSYSTEMS: &[DependencyEcosystem] = &[
    DependencyEcosystem {
        manifest: "Cargo.toml",
        locks: &["Cargo.lock"],
        langs: &[LangId::Rust],
    },
    DependencyEcosystem {
        manifest: "package.json",
        locks: &["package-lock.json", "pnpm-lock.yaml", "yarn.lock"],
        langs: &[LangId::Javascript, LangId::Typescript, LangId::Tsx],
    },
    DependencyEcosystem {
        manifest: "pyproject.toml",
        locks: &["uv.lock", "poetry.lock", "pdm.lock"],
        langs: &[LangId::Python],
    },
    DependencyEcosystem {
        manifest: "Gemfile",
        locks: &["Gemfile.lock"],
        langs: &[LangId::Ruby],
    },
    DependencyEcosystem {
        manifest: "composer.json",
        locks: &["composer.lock"],
        langs: &[LangId::Php],
    },
    DependencyEcosystem {
        manifest: "go.mod",
        locks: &["go.sum"],
        langs: &[LangId::Go],
    },
    DependencyEcosystem {
        // Elixir は astro-sight の解析対象外なので langs は空。
        // lock の候補除外だけが効く (mix.lock は生成物なので言語に依らず除外して良い)。
        manifest: "mix.exs",
        locks: &["mix.lock"],
        langs: &[],
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
    DEPENDENCY_ECOSYSTEMS
        .iter()
        .any(|eco| eco.locks.contains(&base))
}

/// パスが依存宣言ファイル (人が編集する manifest) なら true。
pub fn is_dependency_manifest_path(path: &str) -> bool {
    let Some(base) = base_name(path) else {
        return false;
    };
    DEPENDENCY_ECOSYSTEMS.iter().any(|eco| eco.manifest == base)
}

/// パスが属するエコシステムを返す。manifest でも lock でもなければ `None`。
pub fn ecosystem_for_path(path: &str) -> Option<&'static DependencyEcosystem> {
    let base = base_name(path)?;
    DEPENDENCY_ECOSYSTEMS
        .iter()
        .find(|eco| eco.manifest == base || eco.locks.contains(&base))
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
    DEPENDENCY_ECOSYSTEMS.iter().any(|eco| {
        (base_a == eco.manifest && eco.locks.contains(&base_b))
            || (base_b == eco.manifest && eco.locks.contains(&base_a))
    })
}

/// 依存宣言ファイル (manifest / lock) が、そのソースファイルを配下に持つ位置にあるか。
///
/// monorepo で `apps/web/package.json` と `apps/api/src/main.ts` のように別プロジェクトの
/// 組が作られるのを防ぐ。manifest がリポジトリルートにある場合 (parent が空) は
/// 全ソースの祖先として扱う。
pub fn declaration_covers_source(declaration_path: &str, source_path: &str) -> bool {
    let Some(dir) = std::path::Path::new(declaration_path).parent() else {
        return false;
    };
    std::path::Path::new(source_path).starts_with(dir)
}
