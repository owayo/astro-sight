use anyhow::Result;
use std::fs;
use std::path::PathBuf;

/// BLAKE3 によるコンテンツアドレスファイルキャッシュ。
pub struct CacheStore {
    dir: PathBuf,
}

impl CacheStore {
    pub fn new() -> Result<Self> {
        let dir = cache_dir();
        fs::create_dir_all(&dir)?;
        collect_stale_generations_once();
        Ok(Self { dir })
    }

    /// コンテンツの BLAKE3 ハッシュを算出する。
    pub fn hash(content: &[u8]) -> String {
        blake3::hash(content).to_hex().to_string()
    }

    /// ハッシュキーとコマンドからキャッシュデータを取得する。
    ///
    /// 取得データは呼び出し側が JSON としてそのまま stdout に流すため、
    /// 過去バージョンの非アトミック書き込みで残った truncated ファイルを
    /// 誤配信しないよう、末尾が `}` で閉じていることだけ軽量検証する
    /// (キャッシュ対象の ast / symbols 出力は常に JSON オブジェクト)。
    /// 不正なら miss 扱いにして自己修復のためファイルを削除する。
    pub fn get(&self, hash: &str, command: &str) -> Option<Vec<u8>> {
        let path = self.cache_path(hash, command);
        let data = fs::read(&path).ok()?;
        let valid = data
            .iter()
            .rev()
            .find(|b| !b.is_ascii_whitespace())
            .is_some_and(|b| *b == b'}');
        if !valid {
            let _ = fs::remove_file(&path);
            return None;
        }
        Some(data)
    }

    /// キャッシュにデータを保存する。
    ///
    /// 同一ディレクトリの一時ファイルへ書いてから rename する (同一 FS 上で atomic)。
    /// `fs::write` 直書きだと書き込み途中の中断 (kill / 電源断) で truncated JSON が
    /// 恒久的に残り、同一内容・同一バージョンの間は壊れた応答を返し続けてしまう。
    pub fn put(&self, hash: &str, command: &str, data: &[u8]) -> Result<()> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

        let path = self.cache_path(hash, command);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp_path = path.with_extension(format!(
            "tmp.{}.{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        if let Err(error) = fs::write(&tmp_path, data) {
            let _ = fs::remove_file(&tmp_path);
            return Err(error.into());
        }
        if let Err(e) = fs::rename(&tmp_path, &path) {
            let _ = fs::remove_file(&tmp_path);
            return Err(e.into());
        }
        Ok(())
    }

    /// キャッシュ全体をクリアする。
    pub fn clear(&self) -> Result<()> {
        if self.dir.exists() {
            fs::remove_dir_all(&self.dir)?;
            fs::create_dir_all(&self.dir)?;
        }
        Ok(())
    }

    fn cache_path(&self, hash: &str, command: &str) -> PathBuf {
        // 先頭2文字をディレクトリシャードとして使用
        let (prefix, rest) = hash.split_at(2.min(hash.len()));
        self.dir.join(prefix).join(format!("{rest}.{command}.json"))
    }
}

/// キャッシュのルート。世代ディレクトリの親。
fn cache_root() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".cache")
        .join("astro-sight")
}

/// この astro-sight バージョン専用のキャッシュ世代ディレクトリ名。
fn current_generation() -> String {
    format!("v{}", env!("CARGO_PKG_VERSION"))
}

fn cache_dir() -> PathBuf {
    cache_root().join(current_generation())
}

/// 旧バージョンのキャッシュ世代を削除する (プロセスあたり 1 回、best-effort)。
///
/// キャッシュキーには astro-sight のバージョンが混ざっているため、リリースのたびに
/// 全エントリが失効する。しかし**失効するだけで消えはしない**ので、更新を重ねるほど
/// 死蔵データが積み上がっていた (実測: 開発機で 176MB、そのほぼ全量が到達不能)。
/// 世代ごとにディレクトリを分け、新しい世代の初回起動で兄弟世代を掃く。
///
/// 失敗は無視する — キャッシュの掃除に失敗しても解析は続けられるべきで、
/// 権限や他プロセスの都合で消せないことを利用者に見せる必要がない。
/// 異なるバージョンが同時に走っていると走査中のディレクトリを消しうるが、
/// キャッシュの読み書き失敗は miss として扱われるだけで解析結果は変わらない。
fn collect_stale_generations_once() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| collect_stale_generations_in(&cache_root(), &current_generation()));
}

/// [`collect_stale_generations_once`] の実体。root と現世代名を引数で受けてテスト可能にする。
fn collect_stale_generations_in(root: &std::path::Path, current: &str) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !is_stale_generation_dir(name, current) {
            continue;
        }
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// キャッシュルート直下のディレクトリ名が「消してよい旧世代」か。
///
/// 対象は 2 種類だけ:
/// - 旧バージョンの世代ディレクトリ (`v26.8.110`)
/// - 世代分離を導入する前のフラットな 2 桁 hex シャード (`00`〜`ff`)
///
/// **それ以外の名前は消さない。** このディレクトリは利用者の `~/.cache` 配下にあり、
/// astro-sight が作ったと確証が持てないものを消すと復元できない。判定を「現世代以外の
/// 全部」にしないのはそのため。
fn is_stale_generation_dir(name: &str, current: &str) -> bool {
    if name == current {
        return false;
    }
    let is_generation = name
        .strip_prefix('v')
        .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()));
    let is_legacy_shard = name.len() == 2
        && name
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    is_generation || is_legacy_shard
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト用の隔離された CacheStore を生成
    fn test_store() -> (CacheStore, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let store = CacheStore {
            dir: tmp.path().to_path_buf(),
        };
        (store, tmp)
    }

    /// 同じ入力に対して決定論的に同じハッシュを返すことを検証
    #[test]
    fn hash_deterministic() {
        let h1 = CacheStore::hash(b"hello");
        let h2 = CacheStore::hash(b"hello");
        assert_eq!(h1, h2);
    }

    /// 異なる入力に対して異なるハッシュを返すことを検証
    #[test]
    fn hash_different_inputs() {
        let h1 = CacheStore::hash(b"hello");
        let h2 = CacheStore::hash(b"world");
        assert_ne!(h1, h2);
    }

    /// put で保存したデータを get で正しく取得できることを検証
    /// (キャッシュ対象は ast / symbols の JSON オブジェクト出力)
    #[test]
    fn put_and_get() {
        let (store, _tmp) = test_store();
        let hash = CacheStore::hash(b"test_put_and_get_content");
        store
            .put(&hash, "test_cmd", b"{\"cached\":\"data\"}")
            .unwrap();
        let result = store.get(&hash, "test_cmd");
        assert_eq!(result, Some(b"{\"cached\":\"data\"}".to_vec()));
    }

    /// 末尾が `}` で閉じない truncated キャッシュは miss 扱いで自己削除されることを検証
    /// (旧バージョンの非アトミック書き込みで残った torn write の誤配信防止)
    #[test]
    fn get_rejects_truncated_cache_and_self_heals() {
        let (store, _tmp) = test_store();
        let hash = CacheStore::hash(b"test_truncated_content");
        store
            .put(&hash, "trunc_cmd", b"{\"key\":\"value\"}")
            .unwrap();
        // 書き込み途中で中断された torn write を模倣する
        let path = store.cache_path(&hash, "trunc_cmd");
        fs::write(&path, b"{\"key\":\"val").unwrap();
        assert_eq!(store.get(&hash, "trunc_cmd"), None);
        // 壊れたファイルは削除され、以後も miss のまま
        assert!(!path.exists());
    }

    /// put が一時ファイルを残さないことを検証 (temp + rename の後始末)
    #[test]
    fn put_leaves_no_tmp_files() {
        let (store, tmp) = test_store();
        let hash = CacheStore::hash(b"test_tmp_cleanup");
        store.put(&hash, "tmp_cmd", b"{}").unwrap();
        let mut stack = vec![tmp.path().to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    let name = path.file_name().unwrap().to_string_lossy().into_owned();
                    assert!(!name.contains(".tmp."), "leftover tmp file: {name}");
                }
            }
        }
    }

    /// 存在しないキーに対して None を返すことを検証
    #[test]
    fn get_missing_returns_none() {
        let (store, _tmp) = test_store();
        let result = store.get("nonexistent_hash_for_test", "cmd");
        assert!(result.is_none());
    }

    /// clear 後にキャッシュが空になることを検証
    #[test]
    fn clear_removes_cache() {
        let (store, _tmp) = test_store();
        let hash = CacheStore::hash(b"test_clear_content");
        store.put(&hash, "test_clear", b"{\"d\":1}").unwrap();
        assert!(store.get(&hash, "test_clear").is_some());
        store.clear().unwrap();
        assert!(store.get(&hash, "test_clear").is_none());
    }

    /// 旧世代だけを消し、現世代と「astro-sight が作ったと確証が持てない名前」は残す。
    ///
    /// このディレクトリは利用者の `~/.cache` 配下なので、判定を「現世代以外の全部」に
    /// すると同居している別物を消しうる。対照ケースを同じテストで固定する。
    #[test]
    fn stale_generation_dirs_are_identified_conservatively() {
        let current = "v26.9.100";
        // 消す: 旧世代 + 世代分離前のフラットな hex シャード
        for name in ["v26.8.111", "v1.0.0", "00", "ff", "a3", "9c"] {
            assert!(
                is_stale_generation_dir(name, current),
                "{name} は旧世代として削除対象のはず"
            );
        }
        // 残す: 現世代、および astro-sight が作ったと確証が持てない名前
        for name in [
            current,
            "logs",
            "tmp",
            "vendor",
            "v",
            "version-notes",
            "README",
            "0",
            "000",
            "gg",
            "AB",
        ] {
            assert!(
                !is_stale_generation_dir(name, current),
                "{name} は削除してはならない"
            );
        }
    }

    /// 実ディレクトリに対して旧世代だけが消えることを確認する。
    #[test]
    fn collect_stale_generations_removes_only_old_generations() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let current = "v26.9.100";
        for name in [current, "v26.8.111", "00", "logs"] {
            fs::create_dir_all(root.join(name)).unwrap();
            fs::write(root.join(name).join("entry.json"), b"{}").unwrap();
        }
        collect_stale_generations_in(root, current);
        assert!(root.join(current).exists(), "現世代を消してはならない");
        assert!(root.join("logs").exists(), "未知の名前を消してはならない");
        assert!(!root.join("v26.8.111").exists(), "旧世代は消えるべき");
        assert!(!root.join("00").exists(), "旧フラットシャードは消えるべき");
    }

    /// キャッシュ本体はバージョン別ディレクトリに入る (世代 GC が効く前提)。
    #[test]
    fn cache_dir_is_namespaced_by_version() {
        let dir = cache_dir();
        let generation = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
        assert_eq!(generation, current_generation());
        assert_eq!(dir.parent().map(|p| p.to_path_buf()), Some(cache_root()));
    }
}
