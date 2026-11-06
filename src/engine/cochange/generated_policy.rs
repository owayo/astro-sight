//! cochange 専用の「生成物」判定ポリシー。
//!
//! バッチ処理・コード生成・ビルドが同時に書き出すファイル群は、履歴上ほぼ必ず同一
//! コミットに乗る。cochange はこれを高 confidence の共変更として拾うため、無関係な
//! 人手の変更に対して「このファイルも直すべき」と提示してしまう (実測: cron が毎日
//! 書き出す収集データを起点/推薦先とする推薦が confidence 30〜60% で 5 件)。
//! 「同じバッチが同時に書き出す」という**機械的な同時更新**は、「意味的な依存関係」
//! ではない。
//!
//! # 判定順 (先に決まったものが勝つ)
//!
//! refs / dead-code / API 差分と同じ [`crate::engine::generated::judge`] を使う。
//!
//! 1. `.gitattributes` の `linguist-generated` — 明示宣言。set と `false` 以外の値は除外、
//!    unset / `false` は**名前とマーカーを見ずに**残す (利用者による明示的な上書き)
//! 2. ファイル名の型 (`*.min.js` などの minified・bundled、IDE 補助)
//! 3. ファイル先頭のヘッダマーカー (`@generated` / `DO NOT EDIT` 等) — 自動認識
//! 4. どれでも決まらなければ残す
//!
//! 属性を解決できなかったパス (属性ファイルや index を読めない、リポジトリを開けない) は
//! 除外しない。利用者が `-linguist-generated` で除外を解除しているかを確かめられないので、
//! 名前やマーカーだけで除外すると明示的な解除指定を無視して本物の共変更を消しうる。失敗した
//! 事実は `attr_lookup_failed` で申告する。
//!
//! # 判定しないもの
//!
//! 拡張子・更新頻度・コミット作者では判定しない。人手で更新する JSON と生成された
//! JSON は内容だけでは区別できないため、「マーカーの無い生成 JSON が設定なしでは
//! 認識できない」のは欠陥ではなく識別可能性の限界として受け入れる (利用者は
//! `.gitattributes` で宣言できる)。
//!
//! # AST 解析側の generated 除外とは独立
//!
//! `refs::collect_files` も generated ファイルを除外するが、あちらは「解析対象に
//! しない」判断で、こちらは「共変更の相手として提示しない」判断。生成されたソースは
//! 実行時の影響を持つので AST の影響解析からは外さない (cochange からのみ外す)。
//! **判定と除外対象は独立だが、除外解除の指定 (`--include-generated` /
//! config.toml の `skip_generated`) は共有する** — 「generated を特別扱いしない」という
//! 利用者の意図は両者で共通なので、フラグまで分けると学習コストが増えるだけになる。
//!
//! # 対象状態
//!
//! 属性もマーカーも **解析開始時の worktree** の状態で評価する。履歴上のコミットごとに
//! 「当時 生成物だったか」を復元することはしない (= 「現在 生成物と宣言されている
//! パスの履歴を使わない」という仕様)。

use std::collections::HashMap;
use std::path::Path;

pub(crate) use crate::engine::generated::GeneratedReason;

/// cochange の起点・候補から生成物を落とすためのポリシー。
///
/// パス集合の属性を 1 度に解決し、必要なパスだけヘッダマーカーを読む。判定結果はパス単位で
/// メモ化するので、同じ候補が複数の起点に現れても I/O は 1 回きり。
pub(crate) struct GeneratedPolicy {
    /// 判定済みのパス → 除外理由 (None = 残す)。
    decided: HashMap<String, Option<GeneratedReason>>,
    /// 属性を解決できず、判定せずに残したパスがあった。
    attr_lookup_failed: bool,
}

impl GeneratedPolicy {
    /// 判定を行わない (すべて「残す」) ポリシー。`--include-generated` 指定時に使う。
    pub(crate) fn disabled() -> Self {
        Self {
            decided: HashMap::new(),
            attr_lookup_failed: false,
        }
    }

    /// `paths` (`dir` 相対) について生成物判定をまとめて解決する。
    pub(crate) fn resolve<'a, I>(dir: &str, paths: I) -> Self
    where
        I: IntoIterator<Item = &'a str>,
    {
        use crate::engine::generated::{GeneratedVerdict, judge};

        let mut unique: Vec<&str> = paths.into_iter().collect();
        unique.sort_unstable();
        unique.dedup();
        if unique.is_empty() {
            return Self::disabled();
        }
        let base = Path::new(dir);
        let resolver = crate::engine::gitattributes::AttributeResolver::open(base);
        let mut lookup = resolver.lookup();
        let mut decided = HashMap::with_capacity(unique.len());
        let mut attr_lookup_failed = false;
        for path in unique {
            let full = base.join(path);
            let reason = match judge(lookup.linguist_generated(&full), &full, || {
                crate::engine::generated::is_auto_generated(&full)
            }) {
                GeneratedVerdict::Generated(reason) => Some(reason),
                GeneratedVerdict::NotGenerated => None,
                GeneratedVerdict::Undetermined(_) => {
                    attr_lookup_failed = true;
                    None
                }
            };
            decided.insert(path.to_string(), reason);
        }
        Self {
            decided,
            attr_lookup_failed,
        }
    }

    /// `path` を生成物として除外するか。解決していないパスは常に「残す」。
    pub(crate) fn excluded(&self, path: &str) -> Option<GeneratedReason> {
        self.decided.get(path).copied().flatten()
    }

    /// 属性を解決できなかったパスがあったか (診断で申告する)。
    pub(crate) fn attr_lookup_failed(&self) -> bool {
        self.attr_lookup_failed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(dir)
                .status()
                .expect("git")
                .success(),
            "git {args:?}"
        );
    }

    fn init_repo(dir: &Path) {
        git(dir, &["init", "-q", "-b", "main"]);
        git(dir, &["config", "user.name", "astro-sight-tests"]);
        git(dir, &["config", "user.email", "astro-sight@example.com"]);
    }

    #[test]
    fn disabled_policy_keeps_everything() {
        let policy = GeneratedPolicy::disabled();
        assert_eq!(policy.excluded("anything.json"), None);
        assert!(!policy.attr_lookup_failed());
    }

    /// 属性、名前、マーカーの順で判定し、属性の unset はマーカーと名前を打ち消す。
    /// 非 ASCII のパスも属性どおりに判定する。
    #[test]
    fn resolves_attribute_name_and_marker_in_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(dir.path());
        std::fs::create_dir_all(dir.path().join("data")).expect("mkdir");
        std::fs::create_dir_all(dir.path().join("データ")).expect("mkdir");
        std::fs::write(dir.path().join("data/out.json"), "{}\n").expect("write");
        std::fs::write(
            dir.path().join("kept.yaml"),
            "# @generated by a job\nv: 1\n",
        )
        .expect("write");
        std::fs::write(
            dir.path().join("marked.yaml"),
            "# @generated by a job\nv: 1\n",
        )
        .expect("write");
        std::fs::write(dir.path().join("app.min.js"), "x\n").expect("write");
        std::fs::write(dir.path().join("kept.min.js"), "x\n").expect("write");
        std::fs::write(dir.path().join("hand.yaml"), "v: 1\n").expect("write");
        std::fs::write(
            dir.path().join(".gitattributes"),
            "data/*.json linguist-generated\nkept.yaml -linguist-generated\nkept.min.js -linguist-generated\nデータ/*.yaml linguist-generated\n",
        )
        .expect("write");
        std::fs::write(dir.path().join("データ/生成物.yaml"), "v: 1\n").expect("write");

        let dir_str = dir.path().to_str().expect("utf-8 path");
        let paths = [
            "data/out.json",
            "kept.yaml",
            "marked.yaml",
            "app.min.js",
            "kept.min.js",
            "hand.yaml",
            "データ/生成物.yaml",
        ];
        let policy = GeneratedPolicy::resolve(dir_str, paths);
        assert!(!policy.attr_lookup_failed());
        assert_eq!(
            policy.excluded("data/out.json"),
            Some(GeneratedReason::Attribute)
        );
        assert_eq!(
            policy.excluded("kept.yaml"),
            None,
            "unset はマーカーを打ち消す"
        );
        assert_eq!(
            policy.excluded("marked.yaml"),
            Some(GeneratedReason::Header)
        );
        assert_eq!(policy.excluded("app.min.js"), Some(GeneratedReason::Name));
        assert_eq!(
            policy.excluded("kept.min.js"),
            None,
            "unset は名前を打ち消す"
        );
        assert_eq!(policy.excluded("hand.yaml"), None);
        assert_eq!(
            policy.excluded("データ/生成物.yaml"),
            Some(GeneratedReason::Attribute)
        );
    }

    /// 属性を解決できなかったら、マーカーがあっても除外しない。
    ///
    /// 属性が読めない = 利用者が `-linguist-generated` で除外を解除している可能性を
    /// 確認できない、ということ。マーカーだけで除外すると明示的な解除指定を無視して
    /// 本物の共変更を消しうる。対照として、index を直せば同じファイルがマーカーで除外される
    /// ことも確かめる (「マーカー判定自体が壊れていて常に残る」実装でも通るテストにしない)。
    #[test]
    fn attr_lookup_failure_keeps_everything_even_with_marker() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(dir.path());
        let generated = "gen.yaml";
        std::fs::write(
            dir.path().join(generated),
            "# @generated by a batch job\nv: 1\n",
        )
        .expect("write");
        git(dir.path(), &["add", "-A"]);
        // 壊れた index は読めないので、属性を解決できない。
        let index = dir.path().join(".git").join("index");
        let original = std::fs::read(&index).expect("read index");
        std::fs::write(&index, b"not an index").expect("corrupt index");

        let dir_str = dir.path().to_str().expect("utf-8 path");
        let policy = GeneratedPolicy::resolve(dir_str, [generated]);
        assert!(
            policy.attr_lookup_failed(),
            "壊れた index では属性を解決できないはず (前提が崩れるとこのテストは無意味になる)"
        );
        assert_eq!(
            policy.excluded(generated),
            None,
            "属性を確認できないなら、マーカーがあっても除外しない"
        );

        // 対照: index を戻せば (属性は unspecified) マーカーで除外される。
        std::fs::write(&index, original).expect("restore index");
        let policy = GeneratedPolicy::resolve(dir_str, [generated]);
        assert!(!policy.attr_lookup_failed(), "読める index なら成功する");
        assert_eq!(
            policy.excluded(generated),
            Some(GeneratedReason::Header),
            "属性が unspecified ならマーカーで判定する"
        );
    }
}
