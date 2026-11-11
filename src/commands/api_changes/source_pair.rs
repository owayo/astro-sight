//! api.mod 互換判定器の共通入力 (`CompatibleModSite`) と old/new ソースの遅延取得。
//!
//! 互換判定器は言語ごとに 6 個あり、いずれも同じ 9 項目
//! (`dir` / `base` / `old_path` / `new_path` / `name` / `kind` / `old_sig` / `new_sig` /
//! `lang_id`) を必要とする。位置引数で渡すと `old_path` / `new_path`、`old_sig` / `new_sig`
//! のように同型 `&str` が隣接し、取り違えてもコンパイルが通ってしまうため構造体で束ねる。

use crate::engine::parser;
use crate::engine::parser::SourceBuf;
use crate::language::LangId;
use crate::models::review::CompatibleApiModification;

use super::super::git_input::{GitBlobBatch, git_show_blob};

/// api.mod 候補 1 件の現場情報 (互換判定器の共通入力)。
pub(crate) struct CompatibleModSite<'a> {
    pub(crate) dir: &'a str,
    pub(crate) base: &'a str,
    pub(crate) old_path: &'a str,
    pub(crate) new_path: &'a str,
    pub(crate) name: &'a str,
    pub(crate) kind: &'a str,
    pub(crate) old_sig: &'a str,
    pub(crate) new_sig: &'a str,
    pub(crate) lang_id: Option<LangId>,
}

impl<'a> CompatibleModSite<'a> {
    /// 言語ゲート。`lang_id` が `allowed` に含まれればその言語を返す。
    /// 判定器ごとに対象言語が違う (TS/TSX のみ / +JS / Python) ため許可集合は引数で渡す。
    pub(crate) fn lang_in(&self, allowed: &[LangId]) -> Option<LangId> {
        self.lang_id.filter(|l| allowed.contains(l))
    }

    /// 互換変更 1 件を組み立てる。`file` は常に新側パス。
    pub(crate) fn compatible(&self, reason: &str) -> CompatibleApiModification {
        CompatibleApiModification {
            change_evidence: None,
            inline_module: None,
            name: self.name.to_string(),
            kind: self.kind.to_string(),
            file: self.new_path.to_string(),
            old_signature: Some(self.old_sig.to_string()),
            new_signature: Some(self.new_sig.to_string()),
            reason: reason.to_string(),
        }
    }

    /// old 側 (base リビジョン) と new 側 (working tree) のソースを取得する。
    /// 信頼境界外パスの再チェック → base 側 blob → working tree read の順で、
    /// いずれか失敗すれば `None` (= blocking 維持)。
    ///
    /// `blobs` は検出全体で共有する base 側の読み手。`None` (判定器を単体で呼ぶテスト) は
    /// 単発の `git show` で読む。どちらも同じ blob を返す。
    fn load_sources(&self, blobs: Option<&GitBlobBatch>) -> Option<OldNewSources> {
        // 信頼境界外のパスは多層防御で再チェックする。
        if !crate::engine::impact::is_safe_diff_path(self.old_path)
            || !crate::engine::impact::is_safe_diff_path(self.new_path)
        {
            return None;
        }
        let old = match blobs {
            Some(blobs) => blobs.read(self.old_path)?,
            None => git_show_blob(self.dir, self.base, self.old_path)?,
        };
        let new = load_new_source(self.dir, self.new_path)?;
        Some(OldNewSources::new(old, new))
    }
}

/// working tree 側だけを先に読み、old 側の `git show` が必要か安価に判定できるようにする。
pub(crate) fn load_new_source(dir: &str, new_path: &str) -> Option<SourceBuf> {
    if !crate::engine::impact::is_safe_diff_path(new_path) {
        return None;
    }
    let new_full = std::path::Path::new(dir).join(new_path);
    let new_utf8 = camino::Utf8Path::from_path(&new_full)?;
    parser::read_file(new_utf8).ok()
}

/// 先読み済みの working tree ソースと base 側 blob を組にする。
///
/// 両パスはここでも再検証する。呼び出し側の前段ゲートを信頼境界にしない。
pub(crate) fn load_old_source_with_new(
    base_blobs: &GitBlobBatch,
    old_path: &str,
    new_path: &str,
    new: SourceBuf,
) -> Option<OldNewSources> {
    if !crate::engine::impact::is_safe_diff_path(old_path)
        || !crate::engine::impact::is_safe_diff_path(new_path)
    {
        return None;
    }
    let old = base_blobs.read(old_path)?;
    Some(OldNewSources::new(old, new))
}

/// base 側 blob と working tree ソースの組。
///
/// TS の `stripInternal` が当たる側は、`old` / `new` を宣言ファイルの型のメンバーにしたソース
/// ([`OldNewSources::publish`]) にし、元のソースも持つ。宣言の形の比較は `old` / `new` で、
/// 読み込んだ内容の照合 (在庫と同じ版か) と、宣言の外を変えていないことの字面の比較は元のソース
/// ([`OldNewSources::original_old`] / [`OldNewSources::original_new`]) で行う。
pub(crate) struct OldNewSources {
    pub(crate) old: Vec<u8>,
    pub(crate) new: SourceBuf,
    /// `publish` で書き換える前の旧版。書き換えていなければ None。
    original_old: Option<Vec<u8>>,
    /// `publish` で書き換える前の新版。書き換えていなければ None。
    original_new: Option<SourceBuf>,
}

impl OldNewSources {
    pub(crate) fn new(old: Vec<u8>, new: SourceBuf) -> Self {
        Self {
            old,
            new,
            original_old: None,
            original_new: None,
        }
    }

    /// 元の旧版 (`publish` で型のメンバーを除く前)。
    pub(crate) fn original_old(&self) -> &[u8] {
        self.original_old.as_deref().unwrap_or(&self.old)
    }

    /// 元の新版 (`publish` で型のメンバーを除く前)。
    pub(crate) fn original_new(&self) -> &[u8] {
        self.original_new.as_deref().unwrap_or(&self.new)
    }

    /// `stripInternal` の当たる側を、宣言ファイルの型のメンバーにする (位置は変えない。元の
    /// ソースは残す。Issue #274)。
    pub(crate) fn publish(
        &mut self,
        strip_internal: &super::ts_strip_internal::StripInternal,
        old_path: &str,
        new_path: &str,
        lang: LangId,
    ) {
        // 旧版は base の版の設定で判定する (Issue #287)。
        if let Some(published) = strip_internal.published_old(old_path, lang, &self.old) {
            self.original_old = Some(std::mem::replace(&mut self.old, published));
        }
        if let Some(published) = strip_internal.published(new_path, lang, &self.new) {
            self.original_new = Some(std::mem::replace(&mut self.new, SourceBuf::Vec(published)));
        }
    }

    /// old / new を同一言語で parse したツリー組を返す。
    pub(crate) fn parse_pair(
        &self,
        lang: LangId,
    ) -> Option<(tree_sitter::Tree, tree_sitter::Tree)> {
        let old_tree = parser::parse_source(&self.old, lang).ok()?;
        let new_tree = parser::parse_source(&self.new, lang).ok()?;
        Some((old_tree, new_tree))
    }
}

/// `OldNewSources` の遅延取得 + メモ化。
///
/// 1 シンボルにつき互換判定器が最大 6 個走り、いずれも同じ `(base:old_path, worktree:new_path)`
/// を読む。旧実装は判定器ごとに `git show` を起動していたため 1 件の api.mod で最大 6 プロセスを
/// spawn していた。ここで 1 度だけ取得して使い回す。
///
/// 遅延にするのは、言語ゲートや安価な pre-gate で全判定器が弾かれる場合 (Rust の api.mod 等) に
/// base 側 blob を 1 度も読まない現行挙動を保つため。
///
/// base 側は検出全体で共有する常駐の読み手 (`with_base_blobs`) から読む。シンボルごとに
/// `git show` を起動していた旧実装は、TS 関数 1000 ファイルの引数追加で API 差分フェーズの
/// 大半を占めていた。
///
/// TS の `stripInternal` が当たる側は、宣言ファイルから消える型のメンバーを除いたソースにする
/// ([`OldNewSources::publish`])。判定器は宣言ファイルを使う利用者から見た型を比べる。
#[derive(Default)]
pub(crate) struct SignatureSourceCache<'b> {
    /// base 側 blob の読み手。`None` (判定器を単体で呼ぶテストの既定値) は単発の `git show`。
    base_blobs: Option<&'b GitBlobBatch>,
    /// TS の `stripInternal` の設定。`None` (テストの既定値) は当てない。
    strip_internal: Option<&'b super::ts_strip_internal::StripInternal>,
    /// 外 `None` = 未取得、内 `None` = 取得失敗 (再試行しない)。
    loaded: Option<Option<OldNewSources>>,
}

impl<'b> SignatureSourceCache<'b> {
    pub(crate) fn with_base_blobs(
        base_blobs: &'b GitBlobBatch,
        strip_internal: &'b super::ts_strip_internal::StripInternal,
    ) -> Self {
        Self {
            base_blobs: Some(base_blobs),
            strip_internal: Some(strip_internal),
            loaded: None,
        }
    }

    pub(crate) fn get(&mut self, site: &CompatibleModSite<'_>) -> Option<&OldNewSources> {
        let base_blobs = self.base_blobs;
        let strip_internal = self.strip_internal;
        self.loaded
            .get_or_insert_with(|| {
                let mut sources = site.load_sources(base_blobs)?;
                if let (Some(strip_internal), Some(lang)) = (strip_internal, site.lang_id) {
                    sources.publish(strip_internal, site.old_path, site.new_path, lang);
                }
                Some(sources)
            })
            .as_ref()
    }

    /// 同じ diff のほかのファイル `path` の、base 側の中身。信頼境界の外のパスと、読めない
    /// ときは None。
    pub(crate) fn read_base(&self, site: &CompatibleModSite<'_>, path: &str) -> Option<Vec<u8>> {
        if !crate::engine::impact::is_safe_diff_path(path) {
            return None;
        }
        match self.base_blobs {
            Some(blobs) => blobs.read(path),
            None => git_show_blob(site.dir, site.base, path),
        }
    }
}
