//! 削除した Rust 宣言が Swift / JS から束縛可能だったか、旧ツリー全体の FFI 痕跡を調べる。
//! 推定できない場合は可能性ありとして api.rm を維持する。

use std::cell::OnceCell;

use crate::commands::git_input::GitBlobBatch;
use crate::engine::parser;
use crate::git_support::validate_git_revision;
use crate::language::LangId;

/// 削除した Rust 宣言を、別エコシステムの言語が同じ識別子で束縛し得るか (参照側の言語ごと)。
/// どちらも「旧ツリーに痕跡が無いと確認できた」ときだけ false になる。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ForeignBindability {
    /// Swift (C ABI / uniffi / swift-bridge など)。
    pub(crate) swift: bool,
    /// JS / TS。wasm-bindgen / napi-rs / neon は生成した JS モジュールから同じ識別子で公開し、
    /// C ABI の関数も wasm の `instance.exports.<名前>` として同じ名前で呼べる。
    pub(crate) js: bool,
}

impl ForeignBindability {
    /// 証拠を調べていない / 調べられない場合。どの言語からも束縛され得るとみなす。
    pub(crate) const ANY: Self = Self {
        swift: true,
        js: true,
    };

    /// 参照側の言語から束縛され得るか。痕跡で判定しない言語は true (従来の言語表に任せる)。
    pub(crate) fn allows(self, reference: LangId) -> bool {
        match reference {
            LangId::Swift => self.swift,
            LangId::Javascript | LangId::Typescript | LangId::Tsx => self.js,
            _ => true,
        }
    }

    /// 旧ツリーの痕跡で束縛可否を決める参照側の言語か。
    pub(crate) fn is_evidence_consumer(reference: LangId) -> bool {
        matches!(
            reference,
            LangId::Swift | LangId::Javascript | LangId::Typescript | LangId::Tsx
        )
    }
}

pub(super) struct RustForeignBindingEvidence<'a> {
    dir: &'a str,
    blobs: &'a GitBlobBatch,
    may_bind: OnceCell<ForeignBindability>,
}

impl<'a> RustForeignBindingEvidence<'a> {
    pub(super) fn new(dir: &'a str, blobs: &'a GitBlobBatch) -> Self {
        Self {
            dir,
            blobs,
            may_bind: OnceCell::new(),
        }
    }

    /// 旧ツリー全体に印が無いと確認できた言語だけ false。失敗もキャッシュする。
    pub(super) fn may_bind(&self, old_path: &str) -> ForeignBindability {
        if !old_path.ends_with(".rs") {
            return ForeignBindability::ANY;
        }
        // 明示 diff と base の組み合わせが食い違うと旧ツリーの FFI を見落とす。
        let Some(old_source) = self.blobs.read_limited(old_path, 2 * 1024 * 1024) else {
            return ForeignBindability::ANY;
        };
        if is_lfs_pointer(&old_source) {
            return ForeignBindability::ANY;
        }
        *self.may_bind.get_or_init(|| {
            let mut js = false;
            if self.scan_old_tree(&mut js) {
                ForeignBindability::ANY
            } else {
                ForeignBindability { swift: false, js }
            }
        })
    }

    /// どの言語からも束縛され得る痕跡 (C ABI・ブリッジ・判定不能) があれば true。JS 向けの
    /// バインディング生成 (wasm-bindgen 等) の痕跡だけがある場合は false を返し `js` を立てる。
    fn scan_old_tree(&self, js: &mut bool) -> bool {
        if validate_git_revision(self.blobs.rev(), "base").is_err() {
            return true;
        }
        let root = match crate::git_support::git_command_for_dir(self.dir)
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(self.dir)
            .output()
        {
            Ok(output) if output.status.success() => output.stdout,
            _ => return true,
        };
        let Ok(root) = String::from_utf8(root) else {
            return true;
        };
        let root = root.trim_end_matches('\n');
        let superproject = match crate::git_support::git_command_for_dir(root)
            .args(["rev-parse", "--show-superproject-working-tree"])
            .current_dir(root)
            .output()
        {
            Ok(output) if output.status.success() => output.stdout,
            _ => return true,
        };
        if !superproject.iter().all(|b| *b == b'\n') {
            return true;
        }
        let root_blobs = GitBlobBatch::new(root, self.blobs.rev());
        let output = match crate::git_support::git_command_for_dir(root)
            .args(["ls-tree", "--full-tree", "-r", "-z"])
            .arg(self.blobs.rev())
            .current_dir(root)
            .output()
        {
            Ok(output) if output.status.success() => output.stdout,
            _ => return true,
        };
        let mut paths = Vec::new();
        let mut markdown_links = Vec::new();
        for raw in output.split(|b| *b == 0).filter(|p| !p.is_empty()) {
            let Ok(entry) = std::str::from_utf8(raw) else {
                return true;
            };
            let Some((metadata, path)) = entry.split_once('\t') else {
                return true;
            };
            let mut parts = metadata.split_whitespace();
            let (Some(mode), Some(kind), Some(_oid), None) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                return true;
            };
            if kind == "commit" {
                // サブモジュールの中は、この旧ツリーだけでは読めない。
                return true;
            }
            if mode == "120000" {
                // Markdown 文書同士の別名はソース探索に影響しない。その他のリンクは
                // Cargo path 依存やソースの取り込み先になり得るため保守側に倒す。
                let Some(target) = root_blobs.read_limited(path, 1024) else {
                    return true;
                };
                let Ok(target) = std::str::from_utf8(&target) else {
                    return true;
                };
                if path.ends_with(".md")
                    && target.ends_with(".md")
                    && !target.contains('/')
                    && !target.contains("..")
                {
                    markdown_links.push((path.to_string(), target.to_string()));
                    continue;
                }
                return true;
            }
            if kind != "blob" || !matches!(mode, "100644" | "100755") {
                return true;
            }
            paths.push(path.to_string());
            // 巨大な旧ツリーは走査を打ち切り、従来の blocking を保つ。
            if paths.len() > 10_000 {
                return true;
            }
        }
        for (link, target) in markdown_links {
            let parent = std::path::Path::new(&link)
                .parent()
                .and_then(std::path::Path::to_str)
                .unwrap_or("");
            let resolved = if parent.is_empty() {
                target
            } else {
                format!("{parent}/{target}")
            };
            if !paths.iter().any(|path| path == &resolved) {
                return true;
            }
        }
        if !paths
            .iter()
            .any(|path| path == "Cargo.toml" || path.ends_with("/Cargo.toml"))
        {
            return true;
        }
        for path in paths {
            let file_name = path.rsplit('/').next().unwrap_or(&path);
            if file_name
                .rsplit_once('.')
                .is_some_and(|(_, extension)| !extension.is_ascii())
            {
                // 大文字小文字の折り畳みでソース拡張子になり得る名前は証明しない。
                return true;
            }
            if is_bridge_file(&path) {
                return true;
            }
            let folded = path.to_ascii_lowercase();
            if !folded.ends_with(".rs")
                && !folded.ends_with(".rs.in")
                && !file_name.eq_ignore_ascii_case("cargo.toml")
            {
                continue;
            }
            let Some(content) = root_blobs.read_limited(&path, 2 * 1024 * 1024) else {
                return true;
            };
            if is_lfs_pointer(&content)
                || (folded.ends_with(".rs") || folded.ends_with(".rs.in"))
                    && has_rust_dynamic_path(&content)
                || has_foreign_marker(&content)
            {
                return true;
            }
            if has_js_binding_marker(&content) {
                *js = true;
            }
        }
        false
    }
}

fn is_bridge_file(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    [".udl", ".h", ".hpp", ".hh", ".hxx", ".modulemap", ".def"]
        .iter()
        .any(|ext| path.ends_with(ext))
}

fn has_foreign_marker(content: &[u8]) -> bool {
    let Ok(source) = std::str::from_utf8(content) else {
        return true;
    };
    let compact: String = source.chars().filter(|c| !c.is_whitespace()).collect();
    [
        "extern\"C",
        "extern\"system",
        "no_mangle",
        "export_name",
        "repr(",
        "uniffi",
        "swift_bridge",
        "swift-bridge",
        "swift_rs",
        "swift-rs",
        "cxx::bridge",
        "diplomat",
        "safer_ffi",
        "safer-ffi",
        "flutter_rust_bridge",
        "cbindgen",
        "interoptopus",
        "flapigen",
        "rust_swig",
    ]
    .iter()
    .any(|marker| compact.contains(marker))
}

/// JS から Rust の関数を同じ識別子で呼べるようにするバインディング生成の痕跡
/// (Cargo.toml の依存名、属性、クレートパス)。部分一致で広めに拾う (拾いすぎは blocking 側)。
fn has_js_binding_marker(content: &[u8]) -> bool {
    let Ok(source) = std::str::from_utf8(content) else {
        return true;
    };
    let compact: String = source.chars().filter(|c| !c.is_whitespace()).collect();
    [
        "wasm_bindgen",
        "wasm-bindgen",
        "napi",
        "neon",
        "deno_bindgen",
        "deno-bindgen",
    ]
    .iter()
    .any(|marker| compact.contains(marker))
}

pub(super) fn is_lfs_pointer(content: &[u8]) -> bool {
    content.starts_with(b"version https://git-lfs.github.com/spec/v1\n")
}

fn has_rust_dynamic_path(source: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(source) else {
        return true;
    };
    if !text.contains("path") && !text.contains("include") {
        return false;
    }
    let Ok(tree) = parser::parse_source(source, LangId::Rust) else {
        return true;
    };
    let root = tree.root_node();
    if root.has_error() {
        return true;
    }
    crate::engine::rust_dynamic_source::tree_includes_external_source(root, source)
}

#[cfg(test)]
mod tests {
    use super::{
        GitBlobBatch, RustForeignBindingEvidence, has_foreign_marker, has_rust_dynamic_path,
        is_bridge_file, is_lfs_pointer,
    };
    use std::process::Command;

    #[test]
    fn rust_ffi_markers_are_conservative() {
        assert!(!has_foreign_marker(
            b"pub struct Record { pub value: String }"
        ));
        assert!(has_foreign_marker(b"#[repr ( C )] pub struct Point(i32);"));
        assert!(has_foreign_marker(
            b"#[no_mangle] pub extern \"C\" fn open() {}"
        ));
        assert!(has_foreign_marker(b"uniffi::setup_scaffolding!();"));
        assert!(has_rust_dynamic_path(
            b"include!(concat!(env!(\"OUT_DIR\"), \"/ffi.rs\"));"
        ));
        assert!(has_foreign_marker(&[0xff]));
        assert!(is_bridge_file("src/api.udl"));
        assert!(is_bridge_file("include/api.modulemap"));
        assert!(is_bridge_file("include/API.H"));
        assert!(has_rust_dynamic_path(b"#[path = \"ffi.inc\"] mod ffi;"));
        assert!(has_rust_dynamic_path(
            b"use std::include as load; load!(\"ffi.inc\");"
        ));
        assert!(has_rust_dynamic_path(
            b"use std::{include as load}; load!(\"ffi.inc\");"
        ));
        assert!(has_rust_dynamic_path(b"include!{\"ffi.inc\"}"));
        assert!(has_rust_dynamic_path(b"include![\"ffi.inc\"];"));
        assert!(has_rust_dynamic_path(
            b"#[cfg_attr(all(), path = \"ffi.inc\")] mod ffi;"
        ));
        assert!(!has_rust_dynamic_path(b"fn f() { let path = 1; }"));
        assert!(is_lfs_pointer(
            b"version https://git-lfs.github.com/spec/v1\noid sha256:abc\nsize 1\n"
        ));
    }

    #[test]
    fn missing_old_source_in_base_does_not_prove_absence_of_ffi() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(repo)
                .status()
                .expect("git init")
                .success()
        );
        std::fs::write(repo.join("README.md"), "example\n").expect("write README");
        assert!(
            Command::new("git")
                .args(["add", "README.md"])
                .current_dir(repo)
                .status()
                .expect("git add")
                .success()
        );
        assert!(
            Command::new("git")
                .args([
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=you@example.com",
                    "commit",
                    "-qm",
                    "base",
                ])
                .current_dir(repo)
                .status()
                .expect("git commit")
                .success()
        );
        let path = repo.to_str().expect("utf-8 path");
        let blobs = GitBlobBatch::new(path, "HEAD");
        assert_eq!(
            RustForeignBindingEvidence::new(path, &blobs).may_bind("src/lib.rs"),
            super::ForeignBindability::ANY
        );
    }
}
