//! Cargo の設定と環境が、Rust の依存の取得元とコンパイラを差し替えないことの確認。
//!
//! `rust_reexport_owner` は registry 依存と証明できた外部クレートの別名を情報提供に保つ。
//! Cargo は起動したディレクトリから根までの各祖先の `.cargo/config(.toml)` と
//! `$CARGO_HOME/config(.toml)` を読み、`$CARGO_HOME` の設定はプロジェクトの置き場所に関係なく
//! 効く。どれかに依存の取得元・コンパイラ・`--extern` を変えうるものがあるか、読めない・
//! 解析できないときは証明しない。

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use crate::engine::bounded_read::read_input_text_file;

/// 最上位で許すキー (Cargo の設定のうち、依存の取得元とコンパイラに触れない安定版の表)。
/// 取得元を差し替える `paths`・`patch`・`source`、別のファイルを読み込む `include`、
/// nightly の `unstable`・`host` と未知のキーは許さない。
const TOP_LEVEL_KEYS: &[&str] = &[
    "alias",
    "build",
    "cache",
    "cargo-new",
    "credential-alias",
    "doc",
    "env",
    "future-incompat-report",
    "http",
    "install",
    "net",
    "profile",
    "registries",
    "registry",
    "resolver",
    "target",
    "term",
];

/// `[build]` で許すキー。コンパイラを差し替える `rustc`・`rustc-wrapper`・
/// `rustc-workspace-wrapper` は許さない。
const BUILD_KEYS: &[&str] = &[
    "build-dir",
    "dep-info-basedir",
    "incremental",
    "jobs",
    "rustdoc",
    "rustdocflags",
    "rustflags",
    "target",
    "target-dir",
    "warnings",
];

/// `[target.<triple>]`・`[target.<cfg>]` で許すキー。build script の出力の上書き
/// (`[target.<triple>.<links>]`) は許さない。
const TARGET_KEYS: &[&str] = &["linker", "runner", "rustdocflags", "rustflags"];

/// `[profile.<name>]` (`build-override`・`package.<name>` も) で許す安定版のキー。
const PROFILE_KEYS: &[&str] = &[
    "codegen-units",
    "debug",
    "debug-assertions",
    "incremental",
    "inherits",
    "lto",
    "opt-level",
    "overflow-checks",
    "panic",
    "rpath",
    "split-debuginfo",
    "strip",
];

/// コンパイラを差し替える環境変数と、rustc のプロセスにコードを読み込ませる動的ローダーの
/// 環境変数 (空なら使われない)。
const COMPILER_VARIABLES: &[&str] = &[
    "RUSTC",
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "CARGO_BUILD_RUSTC",
    "CARGO_BUILD_RUSTC_WRAPPER",
    "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
    "LD_PRELOAD",
    "LD_AUDIT",
    "DYLD_INSERT_LIBRARIES",
];

/// 解析の開始時に捕まえた Cargo の環境と、設定ディレクトリごとの判定。
pub(crate) struct CargoConfigProof {
    /// `$CARGO_HOME`。環境変数がコンパイラか rustflags を差し替えうるとき、staged の一時
    /// スナップショットの中を解析するとき (Cargo が読む設定の置き場所が分からない)、
    /// `$CARGO_HOME` を決められないときは `None` で、証明しない。
    home: Option<PathBuf>,
    /// 祖先の走査を止める位置 (`__CARGO_TEST_ROOT`)。
    stop: Option<PathBuf>,
    directories: Mutex<HashMap<PathBuf, Arc<OnceLock<bool>>>>,
}

impl CargoConfigProof {
    /// `root` を解析するこのプロセスの環境から作る。
    pub(crate) fn capture(root: &Path) -> Self {
        let observable = !crate::git_support::is_inside_index_snapshot(root)
            && environment_keeps_compilation(std::env::vars_os());
        let home = observable
            .then(|| {
                cargo_home(
                    std::env::var_os("CARGO_HOME"),
                    || std::env::current_dir().ok(),
                    std::env::home_dir,
                )
            })
            .flatten();
        // cargo-util の `paths::ancestors` と同じく `env::var` で読む (UTF-8 でなければ止めない)。
        Self::new(
            home,
            std::env::var("__CARGO_TEST_ROOT").ok().map(PathBuf::from),
        )
    }

    fn new(home: Option<PathBuf>, stop: Option<PathBuf>) -> Self {
        Self {
            home,
            stop,
            directories: Mutex::new(HashMap::new()),
        }
    }

    /// `path` から根までの祖先 (`path` を含む)。Cargo (cargo-util の `paths::ancestors`) と同じく、
    /// `__CARGO_TEST_ROOT` と字句上等しい祖先を最後にする。正規化して Cargo より早く止めない。
    pub(super) fn ancestors<'a>(&'a self, path: &'a Path) -> impl Iterator<Item = &'a Path> + 'a {
        let mut stopped = false;
        path.ancestors().take_while(move |ancestor| {
            let take = !stopped;
            stopped = stopped || self.stop.as_deref() == Some(*ancestor);
            take
        })
    }

    /// Cargo が `package_root` で読む設定が、どれも依存の取得元とコンパイラを変えないと
    /// 確かめられたか。
    pub(crate) fn keeps_dependency_sources(&self, package_root: &Path) -> bool {
        let Some(home) = &self.home else {
            return false;
        };
        let Some(package_root) = absolute_package_root(package_root) else {
            return false;
        };
        self.ancestors(&package_root)
            .map(|ancestor| ancestor.join(".cargo"))
            .chain(std::iter::once(home.clone()))
            .all(|directory| self.directory_keeps_sources(&directory))
    }

    fn directory_keeps_sources(&self, directory: &Path) -> bool {
        let verdict = self
            .directories
            .lock()
            .expect("Cargo config cache mutex poisoned")
            .entry(directory.to_owned())
            .or_default()
            .clone();
        // 両方あれば Cargo は拡張子なしの `config` だけを読むが、両方とも確かめる。
        *verdict.get_or_init(|| {
            ["config", "config.toml"]
                .iter()
                .all(|name| file_keeps_sources(&directory.join(name)))
        })
    }
}

/// 相対 root でも起動ディレクトリの親まで調べる。canonicalize はせず、祖先と停止位置の
/// 字句上の比較を保つ。
pub(super) fn absolute_package_root(path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        Some(std::env::current_dir().ok()?.join(path))
    }
}

/// Cargo (`home` クレート) と同じ規則の `$CARGO_HOME`。空でない `CARGO_HOME` (相対なら
/// カレントディレクトリから)、無ければホームの `.cargo`。
fn cargo_home(
    variable: Option<OsString>,
    current_dir: impl FnOnce() -> Option<PathBuf>,
    home_dir: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    match variable.filter(|home| !home.is_empty()) {
        Some(home) => {
            let home = PathBuf::from(home);
            if home.is_absolute() {
                Some(home)
            } else {
                Some(current_dir()?.join(home))
            }
        }
        None => Some(home_dir()?.join(".cargo")),
    }
}

/// このプロセスの環境変数 (Cargo と、Cargo が起動する rustc が受け継ぐ) を調べる。
fn environment_keeps_compilation(
    variables: impl IntoIterator<Item = (OsString, OsString)>,
) -> bool {
    variables.into_iter().all(|(name, value)| {
        name.to_str()
            .is_none_or(|name| variable_keeps_compilation(name, value.to_str()))
    })
}

/// 環境変数 1 つが、コンパイラの差し替え・コードの注入・nightly の設定・`--extern` か
/// `@file` の rustflags でないか。`value` は UTF-8 でなければ `None`。
fn variable_keeps_compilation(name: &str, value: Option<&str>) -> bool {
    // Windows の環境変数の名前は大文字小文字を区別しない
    let name = if cfg!(windows) {
        name.to_ascii_uppercase()
    } else {
        name.to_owned()
    };
    if COMPILER_VARIABLES.contains(&name.as_str()) {
        value == Some("")
    } else if name.starts_with("CARGO_UNSTABLE_") {
        false
    } else if name == "RUSTFLAGS" || (name.starts_with("CARGO_") && name.ends_with("_RUSTFLAGS")) {
        // `CARGO_ENCODED_RUSTFLAGS` は 0x1f 区切り
        value.is_some_and(|flags| {
            !flags.contains("--extern")
                && flags
                    .split(|c: char| c.is_whitespace() || c == '\u{1f}')
                    .all(|flag| !flag.starts_with('@'))
        })
    } else {
        true
    }
}

/// `[env]` は Cargo が起動する rustc にも渡るので、プロセスの環境変数と同じ規則で調べる。
fn env_table_keeps_compilation(value: &toml::Value) -> bool {
    value.as_table().is_some_and(|variables| {
        variables.iter().all(|(name, setting)| {
            // 設定による PATH の上書きは、Cargo が起動する rustc の選択を変え得る。
            if name == "PATH" || cfg!(windows) && name.eq_ignore_ascii_case("PATH") {
                return false;
            }
            // `NAME = "value"` か `NAME = { value = "value", force = true }`
            let value = setting
                .as_str()
                .or_else(|| setting.get("value").and_then(toml::Value::as_str));
            variable_keeps_compilation(name, value)
        })
    })
}

/// 設定ファイル 1 つ。無ければ何も変えない。
fn file_keeps_sources(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
        // FIFO などを開いて止まらないよう、通常のファイルだけを読む
        Ok(metadata) => {
            metadata.is_file()
                && read_input_text_file(path).is_some_and(|text| config_keeps_sources(&text))
        }
    }
}

fn config_keeps_sources(text: &str) -> bool {
    let Ok(config) = toml::from_str::<toml::Table>(text) else {
        return false;
    };
    // 文字列のエスケープ (`"--ex\u0074ern"`) を解いた後の値で調べる
    !table_mentions_extern(&config)
        && config.iter().all(|(key, value)| match key.as_str() {
            "env" => env_table_keeps_compilation(value),
            "build" => {
                keys_within(value, BUILD_KEYS)
                    && value.get("rustflags").is_none_or(rustflags_inline)
            }
            "target" => value.as_table().is_some_and(|targets| {
                targets.values().all(|target| {
                    keys_within(target, TARGET_KEYS)
                        && target.get("rustflags").is_none_or(rustflags_inline)
                })
            }),
            "profile" => value
                .as_table()
                .is_some_and(|profiles| profiles.values().all(profile_keeps_sources)),
            key => TOP_LEVEL_KEYS.contains(&key),
        })
}

fn table_mentions_extern(table: &toml::Table) -> bool {
    table
        .iter()
        .any(|(key, value)| key.contains("--extern") || mentions_extern(value))
}

fn mentions_extern(value: &toml::Value) -> bool {
    match value {
        toml::Value::String(text) => text.contains("--extern"),
        toml::Value::Array(items) => items.iter().any(mentions_extern),
        toml::Value::Table(table) => table_mentions_extern(table),
        _ => false,
    }
}

fn keys_within(value: &toml::Value, allowed: &[&str]) -> bool {
    value
        .as_table()
        .is_some_and(|table| table.keys().all(|key| allowed.contains(&key.as_str())))
}

/// rustflags (空白区切りの文字列か文字列の配列) が `@file` で引数を外のファイルから読まないか。
fn rustflags_inline(value: &toml::Value) -> bool {
    match value {
        toml::Value::String(flags) => flags.split_whitespace().all(|flag| !flag.starts_with('@')),
        toml::Value::Array(flags) => flags
            .iter()
            .all(|flag| flag.as_str().is_some_and(|flag| !flag.starts_with('@'))),
        _ => false,
    }
}

fn profile_keeps_sources(profile: &toml::Value) -> bool {
    profile.as_table().is_some_and(|settings| {
        settings.iter().all(|(key, value)| match key.as_str() {
            "build-override" => profile_keeps_sources(value),
            "package" => value
                .as_table()
                .is_some_and(|packages| packages.values().all(profile_keeps_sources)),
            key => PROFILE_KEYS.contains(&key),
        })
    })
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    use super::{
        CargoConfigProof, cargo_home, config_keeps_sources, environment_keeps_compilation,
    };

    const PATCH: &str = "[patch.crates-io]\nanyhow = { path = \"../anyhow\" }\n";

    #[test]
    fn neutral_settings_keep_dependency_sources() {
        for config in [
            "",
            "[build]\njobs = 2\ntarget-dir = \"target\"\nrustflags = \"-C target-cpu=native\"\n[net]\ngit-fetch-with-cli = true\n",
            "build.jobs = 1\n[alias]\nb = \"build\"\n[term]\ncolor = \"always\"\n[env]\nFOO = \"bar\"\n",
            "[registries.internal]\nindex = \"sparse+https://example.com/index/\"\n[registry]\ndefault = \"internal\"\n[http]\nproxy = \"host:3128\"\n",
            "[target.'cfg(windows)']\nrustflags = [\"-C\", \"target-feature=+crt-static\"]\n[target.x86_64-pc-windows-msvc]\nlinker = \"rust-lld.exe\"\nrunner = \"wine\"\n",
            "[profile.dev]\nopt-level = 1\n[profile.dev.package.\"*\"]\nopt-level = 3\n[profile.release.build-override]\ndebug = false\n",
            "[env]\nRUSTUP_TOOLCHAIN = \"1.99.0\"\nLD_LIBRARY_PATH = \"/opt/lib\"\nOPENSSL_DIR = { value = \"vendor/openssl\", relative = true }\nRUSTC_WRAPPER = \"\"\n",
        ] {
            assert!(config_keeps_sources(config), "{config}");
        }
    }

    #[test]
    fn source_compiler_or_unknown_settings_block_the_proof() {
        for config in [
            PATCH,
            // 遠隔のミラーへの置き換えも、連鎖を解決しないので一律に証明しない
            "[source.crates-io]\nreplace-with = \"mirror\"\n[source.mirror]\nregistry = \"sparse+https://mirror.example.com/index/\"\n",
            "paths = [\"../anyhow\"]\n",
            "include = [\"shared.toml\"]\n",
            "[unstable]\nbuild-std = [\"std\"]\n",
            "[host]\nlinker = \"cc\"\n",
            "[made-up]\nkey = 1\n",
            "[build]\nrustc-wrapper = \"sccache\"\n",
            "[build]\nrustc = \"/opt/rustc\"\n",
            "[build]\nrustc-workspace-wrapper = \"wrap\"\n",
            "[build]\nunknown = 1\n",
            "[target.x86_64-unknown-linux-gnu.foo]\nrustc-link-lib = [\"foo\"]\n",
            "[profile.dev]\nrustflags = [\"-Copt-level=1\"]\n",
            "[build]\nrustflags = [\"--extern\", \"anyhow=/tmp/libanyhow.rlib\"]\n",
            "[target.'cfg(unix)']\nrustflags = \"--extern=anyhow=/tmp/libanyhow.rlib\"\n",
            "[build]\nrustflags = [\"--ex\\u0074ern\", \"anyhow=/tmp/libanyhow.rlib\"]\n",
            "[env]\nNOTE = \"--extern anyhow=/tmp/libanyhow.rlib\"\n",
            // `[env]` は rustc にも渡るので、環境変数と同じ規則で調べる
            "[env]\nLD_PRELOAD = { value = \"/tmp/shim.so\", force = true }\n",
            "[env]\nLD_PRELOAD = { force = true }\n",
            "[env]\nRUSTC_WRAPPER = \"sccache\"\n",
            "[env]\nCARGO_UNSTABLE_BUILD_STD = \"std\"\n",
            "[env]\nCARGO_ENCODED_RUSTFLAGS = \"@flags.txt\"\n",
            "env = \"FOO=bar\"\n",
            "[build]\nrustflags = [\"@flags.txt\"]\n",
            "[build]\nrustflags = \"-C opt-level=1 @flags.txt\"\n",
            "[build]\nrustflags = [1]\n",
            "build = 1\n",
            "target = \"x86_64-unknown-linux-gnu\"\n",
            "[patch\n",
        ] {
            assert!(!config_keeps_sources(config), "{config}");
        }
    }

    #[test]
    fn config_path_overrides_block_without_rejecting_process_path() {
        for config in [
            "[env]\nPATH = \"tools\"\n",
            "[env]\nPATH = { value = \"tools\", force = true }\n",
            "[env]\nPATH = { value = \"tools\", relative = true }\n",
            "[env]\nPATH = \"\"\n",
        ] {
            assert!(!config_keeps_sources(config), "{config}");
        }
        for name in ["Path", "pAtH"] {
            assert_eq!(
                config_keeps_sources(&format!(
                    "[env]\n{name} = {{ value = \"tools\", force = true }}\n"
                )),
                !cfg!(windows),
                "{name}"
            );
        }
        assert!(environment_keeps_compilation([
            (OsString::from("PATH"), OsString::from("tools")),
            (OsString::from("Path"), OsString::new()),
        ]));
    }

    /// 走査の外へ出ないよう、一時ディレクトリを止める位置にする。
    fn proof(root: &Path) -> CargoConfigProof {
        CargoConfigProof::new(Some(root.join("home/.cargo")), Some(root.to_path_buf()))
    }

    #[test]
    fn package_ancestors_and_cargo_home_are_read_wherever_the_package_lives() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let package = root.join("work/sample");
        std::fs::create_dir_all(package.join(".cargo")).unwrap();
        std::fs::create_dir_all(root.join("work/.cargo")).unwrap();
        std::fs::create_dir_all(root.join("home/.cargo")).unwrap();
        std::fs::create_dir_all(root.join("home/GitHub/sample")).unwrap();
        assert!(proof(root).keeps_dependency_sources(&package));

        std::fs::write(root.join("work/.cargo/config.toml"), "[build]\njobs = 2\n").unwrap();
        assert!(proof(root).keeps_dependency_sources(&package));
        // 両方あれば Cargo は `config` を読む。どちらの中身も確かめる
        std::fs::write(root.join("work/.cargo/config"), PATCH).unwrap();
        assert!(!proof(root).keeps_dependency_sources(&package));
        std::fs::remove_file(root.join("work/.cargo/config")).unwrap();

        // `$CARGO_HOME` はパッケージの祖先でなくても読み、祖先なら祖先としても読む
        std::fs::write(root.join("home/.cargo/config.toml"), PATCH).unwrap();
        assert!(!proof(root).keeps_dependency_sources(&package));
        assert!(!proof(root).keeps_dependency_sources(&root.join("home/GitHub/sample")));
        std::fs::write(
            root.join("home/.cargo/config.toml"),
            "[net]\noffline = true\n",
        )
        .unwrap();
        assert!(proof(root).keeps_dependency_sources(&package));
        assert!(proof(root).keeps_dependency_sources(&root.join("home/GitHub/sample")));

        // 止める位置より上は読まない
        let below = CargoConfigProof::new(Some(root.join("home/.cargo")), Some(package.clone()));
        std::fs::write(root.join("work/.cargo/config.toml"), PATCH).unwrap();
        assert!(below.keeps_dependency_sources(&package));
        assert!(!proof(root).keeps_dependency_sources(&package));
        std::fs::remove_file(root.join("work/.cargo/config.toml")).unwrap();

        // 読めない・通常のファイルでない・UTF-8 でない・上限を超える設定は証明しない
        std::fs::create_dir(package.join(".cargo/config")).unwrap();
        assert!(!proof(root).keeps_dependency_sources(&package));
        std::fs::remove_dir(package.join(".cargo/config")).unwrap();
        std::fs::write(
            package.join(".cargo/config.toml"),
            b"[build]\njobs = \"\xff\"\n",
        )
        .unwrap();
        assert!(!proof(root).keeps_dependency_sources(&package));
        std::fs::write(package.join(".cargo/config.toml"), "").unwrap();
        assert!(proof(root).keeps_dependency_sources(&package));
        std::fs::OpenOptions::new()
            .write(true)
            .open(package.join(".cargo/config.toml"))
            .and_then(|file| file.set_len(crate::commands::MAX_INPUT_SIZE as u64 + 1))
            .unwrap();
        assert!(!proof(root).keeps_dependency_sources(&package));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_configs_are_followed_like_cargo() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let package = root.join("sample");
        std::fs::create_dir_all(root.join("shared")).unwrap();
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(root.join("shared/config.toml"), PATCH).unwrap();
        std::os::unix::fs::symlink(root.join("shared"), package.join(".cargo")).unwrap();
        assert!(!proof(root).keeps_dependency_sources(&package));
        // 壊れたリンクは Cargo と同じく無いものとして扱う
        std::fs::remove_file(root.join("shared/config.toml")).unwrap();
        std::os::unix::fs::symlink(root.join("missing.toml"), root.join("shared/config")).unwrap();
        assert!(proof(root).keeps_dependency_sources(&package));
    }

    #[test]
    fn unresolved_cargo_home_blocks_the_proof() {
        let root = tempfile::tempdir().unwrap();
        let proof = CargoConfigProof::new(None, Some(root.path().to_path_buf()));
        assert!(!proof.keeps_dependency_sources(root.path()));
    }

    #[test]
    fn relative_analysis_roots_read_parent_overrides() {
        // cwd と環境の変更は、このテストだけを実行する子プロセスに閉じる。
        const ROOT_VARIABLE: &str = "ASTRO_SIGHT_TEST_RELATIVE_CARGO_ROOT";
        const EXPECT_VARIABLE: &str = "ASTRO_SIGHT_TEST_EXPECT_PATCHED_CALLER";
        if let Some(relative) = std::env::var_os(ROOT_VARIABLE) {
            let mut changes = Vec::new();
            crate::engine::impact::analyze_impact_streaming(
                "diff --git a/shapes/src/lib.rs b/shapes/src/lib.rs\n--- a/shapes/src/lib.rs\n+++ b/shapes/src/lib.rs\n@@ -1 +1 @@\n-pub struct Range { pub end: u8 }\n+pub struct Range { pub end: u64 }\n",
                Path::new(&relative),
                &crate::models::impact::ContextAnalysisOptions::default(),
                |impact| {
                    changes.push(impact);
                    Ok(())
                },
            )
            .unwrap();
            let change = changes
                .iter()
                .find(|change| change.path == "shapes/src/lib.rs")
                .unwrap();
            let blocking = change
                .impacted_callers
                .iter()
                .any(|caller| caller.path == "src/lib.rs");
            assert_eq!(
                blocking,
                std::env::var_os(EXPECT_VARIABLE).as_deref() == Some(std::ffi::OsStr::new("1")),
                "{relative:?}: {changes:?}"
            );
            return;
        }

        let mut failures = Vec::new();
        for (parent, expected) in [("neutral", false), ("config", true), ("manifest", true)] {
            let root = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
            let write = |path: &str, text: &str| {
                let path = root.path().join(path);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, text).unwrap();
            };
            write(
                "work/repo/Cargo.toml",
                "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nshapes = \"0.1\"\n",
            );
            write("work/repo/.cargo/config.toml", "[build]\njobs = 2\n");
            write(
                "work/repo/src/lib.rs",
                "use shapes::Range as ShapeRange; pub fn end(value: ShapeRange) -> u8 { value.end }\n",
            );
            write(
                "work/repo/shapes/Cargo.toml",
                "[package]\nname = \"shapes\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            );
            write(
                "work/repo/shapes/src/lib.rs",
                "pub struct Range { pub end: u64 }\n",
            );
            match parent {
                "config" => write(
                    ".cargo/config.toml",
                    "[patch.crates-io]\nshapes = { path = \"work/repo/shapes\" }\n",
                ),
                "manifest" => write(
                    "Cargo.toml",
                    "[workspace]\nmembers = [\"work/repo\", \"work/repo/shapes\"]\n[patch.crates-io]\nshapes = { path = \"work/repo/shapes\" }\n",
                ),
                _ => {}
            }
            for (relative, cwd) in [("repo", "work"), (".", "work/repo")] {
                let mut child = std::process::Command::new(std::env::current_exe().unwrap());
                for (name, _) in std::env::vars_os() {
                    let upper = name.to_string_lossy().to_ascii_uppercase();
                    if upper.starts_with("CARGO_")
                        || upper.starts_with("RUSTC")
                        || matches!(
                            upper.as_str(),
                            "RUSTFLAGS" | "LD_PRELOAD" | "LD_AUDIT" | "DYLD_INSERT_LIBRARIES"
                        )
                    {
                        child.env_remove(name);
                    }
                }
                let output = child
                    .args(["--exact", "engine::impact::rust_cargo_config::tests::relative_analysis_roots_read_parent_overrides", "--nocapture"])
                    .current_dir(root.path().join(cwd))
                    .env(ROOT_VARIABLE, relative)
                    .env(EXPECT_VARIABLE, if expected { "1" } else { "0" })
                    .env("CARGO_HOME", root.path().join("missing-home"))
                    .env("__CARGO_TEST_ROOT", root.path())
                    .output().unwrap();
                if !output.status.success() {
                    failures.push(format!(
                        "{parent}/{relative}: {}{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    ));
                }
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    #[test]
    fn ancestor_walk_stops_at_the_lexically_equal_test_root() {
        let path = Path::new("/r/a/b");
        let walk = |stop: Option<&str>| {
            CargoConfigProof::new(None, stop.map(PathBuf::from))
                .ancestors(path)
                .map(Path::to_path_buf)
                .collect::<Vec<_>>()
        };
        let full = path.ancestors().map(Path::to_path_buf).collect::<Vec<_>>();
        assert_eq!(walk(None), full);
        assert_eq!(walk(Some("/elsewhere")), full);
        assert_eq!(
            walk(Some("/r/a/")),
            [Path::new("/r/a/b"), Path::new("/r/a")]
        );
        assert_eq!(walk(Some("/r/a/b")), [Path::new("/r/a/b")]);
        // 表記の違う同じ場所では止めない (Cargo より早く止めない)
        assert_eq!(walk(Some("/r/b/../a")), full);
        #[cfg(windows)]
        {
            let verbatim = Path::new(r"\\?\C:\r\a\b");
            let walk = |stop: &str| {
                CargoConfigProof::new(None, Some(PathBuf::from(stop)))
                    .ancestors(verbatim)
                    .count()
            };
            assert_eq!(walk(r"C:\r\a"), verbatim.ancestors().count());
            assert_eq!(walk(r"\\?\C:\r\a"), 2);
        }
    }

    #[test]
    fn cargo_home_follows_the_cargo_rules() {
        let absolute = tempfile::tempdir().unwrap();
        let absolute = absolute.path().join("cargo-home");
        let cwd = || Some(PathBuf::from("cwd"));
        let home = || Some(PathBuf::from("user"));
        assert_eq!(
            cargo_home(Some(absolute.clone().into_os_string()), cwd, home),
            Some(absolute)
        );
        assert_eq!(
            cargo_home(Some(OsString::from("relative")), cwd, home),
            Some(Path::new("cwd").join("relative"))
        );
        assert_eq!(
            cargo_home(Some(OsString::new()), cwd, home),
            Some(Path::new("user").join(".cargo"))
        );
        assert_eq!(
            cargo_home(None, cwd, home),
            Some(Path::new("user").join(".cargo"))
        );
        assert_eq!(cargo_home(None, cwd, || None), None);
        assert_eq!(
            cargo_home(Some(OsString::from("relative")), || None, home),
            None
        );
    }

    #[test]
    fn compiler_and_flag_variables_block_the_proof() {
        let keeps = |variables: &[(&str, &str)]| {
            environment_keeps_compilation(
                variables
                    .iter()
                    .map(|(name, value)| (OsString::from(name), OsString::from(value))),
            )
        };
        assert!(keeps(&[]));
        assert!(keeps(&[
            ("RUSTFLAGS", "-C target-cpu=native"),
            ("CARGO_TERM_COLOR", "always"),
            ("CARGO_TARGET_DIR", "target"),
            ("CARGO_BUILD_RUSTDOCFLAGS", "@docs"),
            ("RUSTC_WRAPPER", ""),
            ("LD_PRELOAD", ""),
            // toolchain の選択とライブラリの検索パスは、コンパイラの差し替えとして扱わない
            ("RUSTUP_TOOLCHAIN", "1.99.0"),
            ("LD_LIBRARY_PATH", "/opt/lib"),
        ]));
        for variables in [
            [("RUSTC_WRAPPER", "sccache")],
            [("RUSTC", "/opt/rustc")],
            [("RUSTC_WORKSPACE_WRAPPER", "wrap")],
            [("LD_PRELOAD", "/tmp/shim.so")],
            [("LD_AUDIT", "/tmp/audit.so")],
            [("DYLD_INSERT_LIBRARIES", "/tmp/shim.dylib")],
            [("CARGO_BUILD_RUSTC_WRAPPER", "sccache")],
            [("CARGO_UNSTABLE_BUILD_STD", "std")],
            [("RUSTFLAGS", "--extern anyhow=/tmp/libanyhow.rlib")],
            [("CARGO_BUILD_RUSTFLAGS", "-C opt-level=1 @flags.txt")],
            [(
                "CARGO_ENCODED_RUSTFLAGS",
                "-C\u{1f}opt-level=1\u{1f}@flags.txt",
            )],
            [(
                "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS",
                "--extern=anyhow=/tmp/libanyhow.rlib",
            )],
        ] {
            assert!(!keeps(&variables), "{variables:?}");
        }
        // Windows の環境変数の名前は大文字小文字を区別しない
        assert_eq!(keeps(&[("RustFlags", "--extern anyhow=x")]), !cfg!(windows));
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            assert!(!environment_keeps_compilation([(
                OsString::from("RUSTFLAGS"),
                OsString::from_vec(b"-C\xff".to_vec()),
            )]));
        }
    }

    #[test]
    fn staged_snapshot_does_not_prove_registry_sources() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".git")).unwrap();
        std::fs::write(
            root.path()
                .join(".git")
                .join(crate::git_support::INDEX_SNAPSHOT_MARKER),
            "",
        )
        .unwrap();
        std::fs::create_dir_all(root.path().join("sample")).unwrap();
        let proof = CargoConfigProof::capture(&root.path().join("sample"));
        assert!(!proof.keeps_dependency_sources(&root.path().join("sample")));
    }
}
