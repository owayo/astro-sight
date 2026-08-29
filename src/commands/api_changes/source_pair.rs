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

use super::super::git_input::git_show_blob;

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
            name: self.name.to_string(),
            kind: self.kind.to_string(),
            file: self.new_path.to_string(),
            old_signature: Some(self.old_sig.to_string()),
            new_signature: Some(self.new_sig.to_string()),
            reason: reason.to_string(),
        }
    }

    /// old 側 (base リビジョン) と new 側 (working tree) のソースを取得する。
    /// 信頼境界外パスの再チェック → `git show` → working tree read の順で、
    /// いずれか失敗すれば `None` (= blocking 維持)。
    fn load_sources(&self) -> Option<OldNewSources> {
        load_old_new_sources(self.dir, self.base, self.old_path, self.new_path)
    }
}

/// base 側 blob と working tree ソースを取得する (対象シンボルに依らない)。
///
/// シグネチャ差分に乗らない契約変更 (TypedDict のフィールド requiredness 等) の検出でも
/// 同じ組が要るため、`CompatibleModSite` から切り出して共有する。失敗はすべて `None` で、
/// 呼び出し側は「証明できない = 分類しない」に倒す。
pub(crate) fn load_old_new_sources(
    dir: &str,
    base: &str,
    old_path: &str,
    new_path: &str,
) -> Option<OldNewSources> {
    // 信頼境界外のパスは多層防御で再チェックする。
    if !crate::engine::impact::is_safe_diff_path(old_path)
        || !crate::engine::impact::is_safe_diff_path(new_path)
    {
        return None;
    }
    let old = git_show_blob(dir, base, old_path)?;
    let new_full = std::path::Path::new(dir).join(new_path);
    let new_utf8 = camino::Utf8Path::from_path(&new_full)?;
    let new = parser::read_file(new_utf8).ok()?;
    Some(OldNewSources { old, new })
}

/// base 側 blob と working tree ソースの組。
pub(crate) struct OldNewSources {
    pub(crate) old: Vec<u8>,
    pub(crate) new: SourceBuf,
}

impl OldNewSources {
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
/// `git show` を 1 度も起動しない現行挙動を保つため。
#[derive(Default)]
pub(crate) struct SignatureSourceCache {
    /// 外 `None` = 未取得、内 `None` = 取得失敗 (再試行しない)。
    loaded: Option<Option<OldNewSources>>,
}

impl SignatureSourceCache {
    pub(crate) fn get(&mut self, site: &CompatibleModSite<'_>) -> Option<&OldNewSources> {
        self.loaded
            .get_or_insert_with(|| site.load_sources())
            .as_ref()
    }
}
