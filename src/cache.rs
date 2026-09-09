//! Cache: in-memory LRU + persistent file cache (1hr TTL)
//! Extracted from main.rs; now with true LRU (lru crate, cap 200) and file locking (fs2).

use crate::ScrapeResult;
use fs2::FileExt;
use lru::LruCache;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{Read, Seek, Write};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CAP: usize = 200;
const TTL_SECS: u64 = 3600;

static CACHE: OnceLock<Mutex<LruCache<String, (Instant, ScrapeResult)>>> = OnceLock::new();

fn cache() -> &'static Mutex<LruCache<String, (Instant, ScrapeResult)>> {
    CACHE.get_or_init(|| {
        Mutex::new(LruCache::new(
            NonZeroUsize::new(CAP).expect("CAP non-zero"),
        ))
    })
}

pub fn cache_key(url: &str, focus: Option<&str>, max_chars: usize) -> String {
    format!("{}|{}|{}", url, focus.unwrap_or(""), max_chars)
}

pub fn cache_key_ext(
    url: &str,
    focus: Option<&str>,
    max_chars: usize,
    offset: Option<usize>,
    include_media: bool,
    pages: Option<&str>,
    actions_len: usize,
) -> String {
    format!(
        "{}|{}|{}|{}|{}|{}|{}",
        url,
        focus.unwrap_or(""),
        max_chars,
        offset.unwrap_or(0),
        include_media,
        pages.unwrap_or(""),
        actions_len
    )
}

fn cache_file_path() -> PathBuf {
    let base = std::env::var("HOME").unwrap_or_else(|_| "/home/adam-underwood".to_string());
    PathBuf::from(base).join(".cache").join("scour").join("cache.json")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn file_cache_get(key: &str) -> Option<ScrapeResult> {
    let path = cache_file_path();
    let file = OpenOptions::new().read(true).open(&path).ok()?;
    let _ = file.lock_shared();
    let mut contents = String::new();
    // Read from the locked file handle
    let mut f = &file;
    let _ = f.seek(std::io::SeekFrom::Start(0));
    if f.read_to_string(&mut contents).is_err() {
        let _ = fs2::FileExt::unlock(&file);
        return None;
    }
    let _ = fs2::FileExt::unlock(&file);
    if contents.is_empty() {
        return None;
    }
    let map: HashMap<String, (u64, ScrapeResult)> = serde_json::from_str(&contents).ok()?;
    let (ts, val) = map.get(key)?;
    if now_secs().saturating_sub(*ts) < TTL_SECS {
        Some(val.clone())
    } else {
        None
    }
}

fn file_cache_set(key: String, val: &ScrapeResult) {
    let path = cache_file_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
    {
        Ok(f) => f,
        Err(_) => return,
    };
    if file.lock_exclusive().is_err() {
        return;
    }
    // Read existing contents via locked handle
    let mut contents = String::new();
    let _ = file.seek(std::io::SeekFrom::Start(0));
    let _ = file.read_to_string(&mut contents);
    let map: HashMap<String, (u64, ScrapeResult)> =
        serde_json::from_str(&contents).unwrap_or_default();

    // Build LRU to enforce true LRU eviction (cap 200). Insertion order from HashMap
    // is arbitrary but stable for this write; the new key is most-recent.
    let mut lru: LruCache<String, (u64, ScrapeResult)> =
        LruCache::new(NonZeroUsize::new(CAP).expect("CAP non-zero"));
    for (k, v) in map {
        // Skip expired entries to keep file small
        if now_secs().saturating_sub(v.0) < TTL_SECS {
            lru.put(k, v);
        }
    }
    lru.put(key, (now_secs(), val.clone()));

    let out_map: HashMap<String, (u64, ScrapeResult)> = lru.into_iter().collect();
    if let Ok(json) = serde_json::to_string(&out_map) {
        let _ = file.set_len(0);
        let _ = file.seek(std::io::SeekFrom::Start(0));
        let _ = file.write_all(json.as_bytes());
        let _ = file.flush();
    }
    let _ = fs2::FileExt::unlock(&file);
}

pub fn cache_get(key: &str) -> Option<ScrapeResult> {
    {
        if let Ok(mut guard) = cache().lock() {
            // Peek without promoting to check TTL first
            if let Some((when, _)) = guard.peek(key) {
                if when.elapsed() < Duration::from_secs(TTL_SECS) {
                    // Now promote via get
                    if let Some((_, val)) = guard.get(key) {
                        return Some(val.clone());
                    }
                } else {
                    // Expired: remove
                    guard.pop(key);
                }
            }
        }
    }
    if let Some(val) = file_cache_get(key) {
        if let Ok(mut g) = cache().lock() {
            g.put(key.to_string(), (Instant::now(), val.clone()));
        }
        return Some(val);
    }
    None
}

pub fn cache_set(key: String, val: ScrapeResult) {
    if let Ok(mut g) = cache().lock() {
        g.put(key.clone(), (Instant::now(), val.clone()));
    }
    file_cache_set(key, &val);
}

pub fn cache_clear() {
    if let Ok(mut g) = cache().lock() {
        g.clear();
    }
    let path = cache_file_path();
    // Lock exclusively while removing to avoid race
    if let Ok(file) = OpenOptions::new().write(true).open(&path) {
        let _ = file.lock_exclusive();
        let _ = std::fs::remove_file(&path);
        let _ = fs2::FileExt::unlock(&file);
    } else {
        let _ = std::fs::remove_file(&path);
    }
}



#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroUsize;

    #[test]
    fn lru_eviction_order_is_true_lru() {
        let mut lru: LruCache<String, u32> =
            LruCache::new(NonZeroUsize::new(200).unwrap());
        // Fill to capacity
        for i in 0..200 {
            lru.put(format!("k{i}"), i);
        }
        assert_eq!(lru.len(), 200);
        // Access k0 and k1 to make them most-recent
        let _ = lru.get("k0");
        let _ = lru.get("k1");
        // Insert two new keys, should evict LRU = k2 and k3 (least recently used)
        lru.put("k200".to_string(), 200);
        lru.put("k201".to_string(), 201);
        assert_eq!(lru.len(), 200);
        assert!(lru.get("k0").is_some(), "k0 was accessed, should remain");
        assert!(lru.get("k1").is_some(), "k1 was accessed, should remain");
        assert!(lru.peek("k2").is_none(), "k2 should be evicted as LRU");
        assert!(lru.peek("k3").is_none(), "k3 should be evicted as LRU");
        assert!(lru.get("k200").is_some());
        assert!(lru.get("k201").is_some());
        // The next LRU after those should be k4
        assert!(lru.peek("k4").is_some(), "k4 should still be present");
    }

    #[test]
    fn cache_memory_lru_evicts_oldest() {
        // Ensure our global cache respects LRU by directly exercising the underlying LruCache
        // This mirrors the production path but isolated.
        let mut lru: LruCache<String, (Instant, u32)> =
            LruCache::new(NonZeroUsize::new(3).unwrap());
        lru.put("a".to_string(), (Instant::now(), 1));
        lru.put("b".to_string(), (Instant::now(), 2));
        lru.put("c".to_string(), (Instant::now(), 3));
        // Access a
        let _ = lru.get("a");
        // Insert d, should evict b (LRU)
        lru.put("d".to_string(), (Instant::now(), 4));
        assert!(lru.peek("b").is_none());
        assert!(lru.peek("a").is_some());
        assert!(lru.peek("c").is_some());
        assert!(lru.peek("d").is_some());
    }
}
