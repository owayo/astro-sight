use anyhow::Result;
use std::fs;
use std::path::PathBuf;

/// Content-addressed file cache using BLAKE3.
pub struct CacheStore {
    dir: PathBuf,
}

impl CacheStore {
    pub fn new() -> Result<Self> {
        let dir = cache_dir();
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    /// Compute the BLAKE3 hash of the given content.
    pub fn hash(content: &[u8]) -> String {
        blake3::hash(content).to_hex().to_string()
    }

    /// Get cached data for a given hash key and command.
    pub fn get(&self, hash: &str, command: &str) -> Option<Vec<u8>> {
        let path = self.cache_path(hash, command);
        fs::read(&path).ok()
    }

    /// Store data in the cache.
    pub fn put(&self, hash: &str, command: &str, data: &[u8]) -> Result<()> {
        let path = self.cache_path(hash, command);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, data)?;
        Ok(())
    }

    /// Clear the entire cache.
    pub fn clear(&self) -> Result<()> {
        if self.dir.exists() {
            fs::remove_dir_all(&self.dir)?;
            fs::create_dir_all(&self.dir)?;
        }
        Ok(())
    }

    fn cache_path(&self, hash: &str, command: &str) -> PathBuf {
        // Use first 2 chars as directory shard
        let (prefix, rest) = hash.split_at(2.min(hash.len()));
        self.dir.join(prefix).join(format!("{rest}.{command}.json"))
    }
}

fn cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("astro-sight")
}
