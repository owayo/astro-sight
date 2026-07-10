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

fn cache_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".cache")
        .join("astro-sight")
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
}
