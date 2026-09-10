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
//! 1. `.gitattributes` の `linguist-generated` — 明示宣言。`set` / `true` は除外、
//!    `unset` / `false` は**マーカーを見ずに**残す (利用者による明示的な上書き)
//! 2. ファイル先頭のヘッダマーカー (`@generated` / `DO NOT EDIT` 等) — 自動認識
//! 3. どちらでも決まらなければ残す
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
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// 生成物と判定した根拠。診断出力の理由づけに使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GeneratedReason {
    /// `.gitattributes` の `linguist-generated` が set / true。
    GitAttribute,
    /// ファイル先頭のヘッダコメントが生成物を宣言している。
    HeaderMarker,
}

/// `git check-attr` が返す `linguist-generated` の状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttrState {
    /// `set` / `true` — 生成物と宣言されている。
    Generated,
    /// `unset` / `false` — 生成物ではないと明示されている (マーカーより優先)。
    NotGenerated,
    /// `unspecified`、または解釈できない値。マーカー判定へ進む。
    Unspecified,
}

/// cochange の起点・候補から生成物を落とすためのポリシー。
///
/// パス集合に対して `.gitattributes` を 1 度だけバッチ問い合わせし、必要なパスだけ
/// ヘッダマーカーを読む。判定結果はパス単位でメモ化するので、同じ候補が複数の起点に
/// 現れても I/O は 1 回きり。
pub(crate) struct GeneratedPolicy {
    /// 判定済みのパス → 除外理由 (None = 残す)。
    decided: HashMap<String, Option<GeneratedReason>>,
    /// `git check-attr` の実行に失敗した (属性を一切参照できなかった)。
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

    /// `paths` について生成物判定をまとめて解決する。
    ///
    /// **`git check-attr` に失敗したら判定そのものを行わない** (全パスを「残す」)。
    /// 属性が読めないということは、利用者が `-linguist-generated` で除外を解除している
    /// 可能性を確認できないということなので、マーカーだけで除外すると明示的な解除指定を
    /// 無視して本物の共変更を消しうる。「判定できないことを理由に候補を消さない」は
    /// `collect_base_tree_paths` (削除済み候補のフィルタ) と同じ方針。
    /// 失敗した事実は `attr_lookup_failed` で申告する。
    pub(crate) fn resolve<'a, I>(dir: &str, paths: I) -> Self
    where
        I: IntoIterator<Item = &'a str>,
    {
        let mut unique: Vec<&str> = paths.into_iter().collect();
        unique.sort_unstable();
        unique.dedup();
        if unique.is_empty() {
            return Self::disabled();
        }

        let Some(attrs) = query_linguist_generated(dir, &unique) else {
            return Self {
                decided: HashMap::new(),
                attr_lookup_failed: true,
            };
        };

        let mut decided = HashMap::with_capacity(unique.len());
        for path in unique {
            let state = attrs.get(path).copied().unwrap_or(AttrState::Unspecified);
            let reason = match state {
                AttrState::Generated => Some(GeneratedReason::GitAttribute),
                // 明示的に「生成物ではない」と宣言されていればマーカーは見ない。
                AttrState::NotGenerated => None,
                AttrState::Unspecified => {
                    if crate::engine::generated::is_auto_generated(&Path::new(dir).join(path)) {
                        Some(GeneratedReason::HeaderMarker)
                    } else {
                        None
                    }
                }
            };
            decided.insert(path.to_string(), reason);
        }
        Self {
            decided,
            attr_lookup_failed: false,
        }
    }

    /// `path` を生成物として除外するか。解決していないパスは常に「残す」。
    pub(crate) fn excluded(&self, path: &str) -> Option<GeneratedReason> {
        self.decided.get(path).copied().flatten()
    }

    /// `git check-attr` に失敗したか (診断で申告する)。
    pub(crate) fn attr_lookup_failed(&self) -> bool {
        self.attr_lookup_failed
    }
}

/// `git check-attr -z --stdin linguist-generated` をバッチ実行する。
///
/// パスは `--stdin` から NUL 区切りで渡すので ARG_MAX を超えない。出力も `-z` で
/// NUL 区切りになり、非 ASCII ファイル名が 8 進クォートされる問題を避けられる。
/// `current_dir(dir)` 基準で解決されるため、astro-sight の `--dir` 相対規約とそのまま
/// 一致する。
///
/// 実行できなかった場合は `None` を返す。呼び出し側は**マーカー判定へ進まず生成物の
/// 除外そのものを諦める** (`GeneratedPolicy::resolve` の doc 参照)。
fn query_linguist_generated(dir: &str, paths: &[&str]) -> Option<HashMap<String, AttrState>> {
    let mut child = Command::new("git")
        .args(["check-attr", "-z", "--stdin", "linguist-generated"])
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    // stdin を閉じてから wait する。パス数が多いと git 側の出力バッファが埋まって
    // 書き込みがブロックしうるため、書き込みは別スレッドに逃がす。
    let mut stdin = child.stdin.take()?;
    let payload: Vec<u8> = paths.iter().fold(Vec::new(), |mut buf, p| {
        buf.extend_from_slice(p.as_bytes());
        buf.push(0);
        buf
    });
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&payload);
        // drop で閉じる
    });
    let output = child.wait_with_output().ok();
    let _ = writer.join();
    let output = output?;
    if !output.status.success() {
        return None;
    }
    Some(parse_check_attr_z(&output.stdout))
}

/// `git check-attr -z` の出力 (`<path>NUL<attr>NUL<value>NUL` の繰り返し) を解釈する。
///
/// レコードは NUL 終端なので、split すると必ず末尾に空要素が 1 つ残る。空の `path` は
/// そこで打ち切り、空の `value` は「レコードが途中で切れた」とみなして捨てる
/// (git は必ず `set` / `unset` / `unspecified` / 値のいずれかを返すため、正常な出力で
/// 空 value は現れない)。壊れた出力を黙って判定に使わない。
fn parse_check_attr_z(stdout: &[u8]) -> HashMap<String, AttrState> {
    let mut out = HashMap::new();
    let mut fields = stdout.split(|&b| b == 0);
    while let (Some(path), Some(_attr), Some(value)) = (fields.next(), fields.next(), fields.next())
    {
        // 末尾の NUL による空要素で打ち切る (path が空 = 実データではない)。
        if path.is_empty() {
            break;
        }
        if value.is_empty() {
            continue;
        }
        let path = String::from_utf8_lossy(path).into_owned();
        out.insert(path, attr_state_from_value(&String::from_utf8_lossy(value)));
    }
    out
}

/// `linguist-generated` の値を状態へ写す。
///
/// Linguist は属性の有無 (`set` / `unset`) と明示値 (`true` / `false`) の両方を受け付ける。
/// 未知の値は「判定不能」として残す側 (`Unspecified`) に倒す — 除外方向へ倒すと、
/// 意味の分からない宣言を理由に本物の共変更を消してしまう。
fn attr_state_from_value(value: &str) -> AttrState {
    match value {
        "set" | "true" => AttrState::Generated,
        "unset" | "false" => AttrState::NotGenerated,
        _ => AttrState::Unspecified,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attr_values_map_to_states() {
        assert_eq!(attr_state_from_value("set"), AttrState::Generated);
        assert_eq!(attr_state_from_value("true"), AttrState::Generated);
        assert_eq!(attr_state_from_value("unset"), AttrState::NotGenerated);
        assert_eq!(attr_state_from_value("false"), AttrState::NotGenerated);
        assert_eq!(attr_state_from_value("unspecified"), AttrState::Unspecified);
        // 未知の値は「残す」側へ倒す (除外方向に倒さない)。
        assert_eq!(attr_state_from_value("linguist"), AttrState::Unspecified);
        assert_eq!(attr_state_from_value(""), AttrState::Unspecified);
    }

    #[test]
    fn parses_nul_separated_check_attr_output() {
        let raw = b"a.json\0linguist-generated\0set\0b.py\0linguist-generated\0unspecified\0";
        let parsed = parse_check_attr_z(raw);
        assert_eq!(parsed.get("a.json"), Some(&AttrState::Generated));
        assert_eq!(parsed.get("b.py"), Some(&AttrState::Unspecified));
        assert_eq!(parsed.len(), 2);
    }

    /// 壊れた (3 要素揃わない) 出力で誤った判定を作らない。
    #[test]
    fn ignores_truncated_trailing_record() {
        let raw = b"a.json\0linguist-generated\0set\0b.py\0linguist-generated\0";
        let parsed = parse_check_attr_z(raw);
        assert_eq!(parsed.get("a.json"), Some(&AttrState::Generated));
        assert_eq!(parsed.get("b.py"), None, "3 要素揃わない末尾は捨てる");
    }

    /// 非 ASCII のファイル名が 8 進クォートされずそのまま復元できる (`-z` の効果)。
    #[test]
    fn preserves_non_ascii_paths() {
        let raw = "データ/生成物.yaml\0linguist-generated\0set\0".as_bytes();
        let parsed = parse_check_attr_z(raw);
        assert_eq!(
            parsed.get("データ/生成物.yaml"),
            Some(&AttrState::Generated)
        );
    }

    #[test]
    fn disabled_policy_keeps_everything() {
        let policy = GeneratedPolicy::disabled();
        assert_eq!(policy.excluded("anything.json"), None);
        assert!(!policy.attr_lookup_failed());
    }

    /// `git check-attr` に失敗したら、マーカーがあっても除外しない。
    ///
    /// 属性が読めない = 利用者が `-linguist-generated` で除外を解除している可能性を
    /// 確認できない、ということ。マーカーだけで除外すると明示的な解除指定を無視して
    /// 本物の共変更を消しうる。対照として、同じファイルが git 管理下では除外されることも
    /// 確かめる (「マーカー判定自体が壊れていて常に残る」実装でも通るテストにしない)。
    #[test]
    fn attr_lookup_failure_keeps_everything_even_with_marker() {
        let dir = tempfile::tempdir().expect("tempdir");
        let generated = "gen.yaml";
        std::fs::write(
            dir.path().join(generated),
            "# @generated by a batch job\nv: 1\n",
        )
        .expect("write");

        // git 管理外なので check-attr が失敗する。
        let dir_str = dir.path().to_str().expect("utf-8 path");
        let policy = GeneratedPolicy::resolve(dir_str, [generated]);
        assert!(
            policy.attr_lookup_failed(),
            "git 管理外では check-attr が失敗するはず (前提が崩れるとこのテストは無意味になる)"
        );
        assert_eq!(
            policy.excluded(generated),
            None,
            "属性を確認できないなら、マーカーがあっても除外しない"
        );

        // 対照: git 管理下 (属性は unspecified) ならマーカーで除外される。
        for args in [
            vec!["init", "-b", "main"],
            vec!["config", "user.name", "astro-sight-tests"],
            vec!["config", "user.email", "astro-sight@example.com"],
        ] {
            assert!(
                Command::new("git")
                    .args(&args)
                    .current_dir(dir.path())
                    .status()
                    .expect("git")
                    .success()
            );
        }
        let policy = GeneratedPolicy::resolve(dir_str, [generated]);
        assert!(!policy.attr_lookup_failed(), "git 管理下なら成功する");
        assert_eq!(
            policy.excluded(generated),
            Some(GeneratedReason::HeaderMarker),
            "属性が unspecified ならマーカーで判定する"
        );
    }
}
