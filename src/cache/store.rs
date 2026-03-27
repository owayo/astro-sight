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
    pub fn get(&self, hash: &str, command: &str) -> Option<Vec<u8>> {
        let path = self.cache_path(hash, command);
        fs::read(&path).ok()
    }

    /// キャッシュにデータを保存する。
    pub fn put(&self, hash: &str, command: &str, data: &[u8]) -> Result<()> {
        let path = self.cache_path(hash, command);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, data)?;
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
    #[test]
    fn put_and_get() {
        let store = CacheStore::new().unwrap();
        let hash = CacheStore::hash(b"test_put_and_get_content");
        store.put(&hash, "test_cmd", b"cached data").unwrap();
        let result = store.get(&hash, "test_cmd");
        assert_eq!(result, Some(b"cached data".to_vec()));
    }

    /// 存在しないキーに対して None を返すことを検証
    #[test]
    fn get_missing_returns_none() {
        let store = CacheStore::new().unwrap();
        let result = store.get("nonexistent_hash_for_test", "cmd");
        assert!(result.is_none());
    }

    /// clear 後にキャッシュが空になることを検証
    #[test]
    fn clear_removes_cache() {
        let store = CacheStore::new().unwrap();
        let hash = CacheStore::hash(b"test_clear_content");
        store.put(&hash, "test_clear", b"data").unwrap();
        assert!(store.get(&hash, "test_clear").is_some());
        store.clear().unwrap();
        assert!(store.get(&hash, "test_clear").is_none());
    }
}
