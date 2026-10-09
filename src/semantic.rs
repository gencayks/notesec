//! Semantic search (decision 43): block embeddings, their cache inside the
//! graph, and ranking by cosine similarity. Pure apart from the cache file;
//! the HTTP call is `ai::embed`, and the app runs both off the UI thread.
//!
//! *What is embedded*: every non-empty block, as `"<page title>\n<block
//! text>"` (the title gives a short block its context), cut to
//! `MAX_EMBED_CHARS`. *Cache*: `<graph>/.notesec/embeddings.json`, keyed
//! by provider + base URL + model (`CacheKey`; a different key throws the
//! old vectors away) and, per block, by an FNV-1a hash of the embedded
//! text, so an edit re-embeds only the blocks that changed and block ids
//! (regenerated on load) don't matter. The folder holds its own
//! `.gitignore` (`*`), so git auto-backup skips it without touching
//! `backup::IGNORED`; nothing under it is ever loaded as a page, searched,
//! exported or trashed (those only read `pages/` and `journals/`).

use crate::model::Page;
use crate::storage::write_atomic;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// The graph's folder for app data that isn't notes.
pub const DATA_DIR: &str = ".notesec";
/// The cache file in `DATA_DIR`.
pub const CACHE_FILE: &str = "embeddings.json";
/// Bumped when the file format changes; other versions are ignored.
const VERSION: u32 = 1;
/// Longest text (in characters) sent to the embedding model per block.
pub const MAX_EMBED_CHARS: usize = 2000;

/// Which vectors a cache holds: they are only comparable with vectors from
/// the same provider, server and model.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheKey {
    pub provider: String,
    pub base: String,
    pub model: String,
}

/// The embedding cache: text hash -> vector, for one `CacheKey`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Cache {
    pub version: u32,
    pub key: CacheKey,
    /// Sorted, so the file only changes where vectors did.
    pub vectors: BTreeMap<String, Vec<f32>>,
}

impl Cache {
    pub fn path(root: &Path) -> PathBuf {
        root.join(DATA_DIR).join(CACHE_FILE)
    }

    /// The cache in `root`; missing, unreadable, invalid or another
    /// version: empty (it is only a cache, so nothing is backed up).
    pub fn load(root: &Path) -> Cache {
        fs::read_to_string(Self::path(root))
            .ok()
            .and_then(|text| serde_json::from_str::<Cache>(&text).ok())
            .filter(|c| c.version == VERSION)
            .unwrap_or_default()
    }

    /// Write atomically, creating `.notesec/` with a `.gitignore` that
    /// keeps the folder out of git (auto-backup, decision 39).
    pub fn save(&self, root: &Path) -> io::Result<()> {
        let dir = root.join(DATA_DIR);
        fs::create_dir_all(&dir)?;
        let ignore = dir.join(".gitignore");
        if !ignore.exists() {
            write_atomic(
                &ignore,
                "# notesec's caches: rebuilt when missing, never committed\n*\n",
            )?;
        }
        let mut copy = self.clone();
        copy.version = VERSION;
        let text = serde_json::to_string(&copy)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        write_atomic(&Self::path(root), &text)
    }

    /// Switch to `key`. A different key drops every vector (they came from
    /// another model or server). Returns whether that happened.
    pub fn use_key(&mut self, key: &CacheKey) -> bool {
        if self.key == *key {
            return false;
        }
        self.key = key.clone();
        self.vectors.clear();
        true
    }

    /// The items whose text has no vector yet, one per distinct hash.
    pub fn missing<'a>(&self, items: &'a [Item]) -> Vec<&'a Item> {
        let mut seen = HashSet::new();
        items
            .iter()
            .filter(|item| !self.vectors.contains_key(&item.hash) && seen.insert(&item.hash))
            .collect()
    }

    /// Forget vectors of texts that are gone (keeps the file small).
    pub fn prune(&mut self, items: &[Item]) {
        let keep: HashSet<&str> = items.iter().map(|i| i.hash.as_str()).collect();
        self.vectors.retain(|hash, _| keep.contains(hash.as_str()));
    }
}

/// One block as semantic search sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub page: usize,
    pub block: usize,
    pub id: Uuid,
    pub title: String,
    pub content: String,
    /// What is embedded (`embed_text`).
    pub text: String,
    /// `text_hash(text)`: the cache key.
    pub hash: String,
}

/// Every non-empty block of `pages`.
pub fn items(pages: &[Page]) -> Vec<Item> {
    pages
        .iter()
        .enumerate()
        .flat_map(|(p, page)| {
            page.blocks
                .iter()
                .enumerate()
                .filter(|(_, block)| !block.content.trim().is_empty())
                .map(move |(b, block)| {
                    let text = embed_text(&page.title, &block.content);
                    Item {
                        page: p,
                        block: b,
                        id: block.id,
                        title: page.title.clone(),
                        content: block.content.clone(),
                        hash: text_hash(&text),
                        text,
                    }
                })
        })
        .collect()
}

/// The text embedded for a block: its page title, a line break, its text,
/// at most `MAX_EMBED_CHARS` characters.
pub fn embed_text(title: &str, content: &str) -> String {
    format!("{title}\n{}", content.trim())
        .chars()
        .take(MAX_EMBED_CHARS)
        .collect()
}

/// FNV-1a (64 bit) of `text`, as 16 hex digits: stable across runs and
/// Rust versions (std's hasher promises neither).
pub fn text_hash(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Cosine similarity; 0 for vectors of different length or zero length.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

/// Items (by index) with a vector, most similar to `query` first, as
/// `(similarity, item)`.
pub fn rank_blocks(query: &[f32], items: &[Item], cache: &Cache) -> Vec<(f32, usize)> {
    let mut scored: Vec<(f32, usize)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, item)| cache.vectors.get(&item.hash).map(|v| (cosine(query, v), i)))
        .collect();
    // Stable: equal scores keep document order.
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored
}

/// Pages ranked by their best block (`(similarity, that block's item)`),
/// at most `limit`.
pub fn rank_pages(query: &[f32], items: &[Item], cache: &Cache, limit: usize) -> Vec<(f32, usize)> {
    let mut seen: HashMap<usize, ()> = HashMap::new();
    rank_blocks(query, items, cache)
        .into_iter()
        .filter(|(_, i)| seen.insert(items[*i].page, ()).is_none())
        .take(limit)
        .collect()
}

/// Each page's vector (by page index): the mean of its blocks' unit
/// vectors, blocks without a cached vector (or of another length) left
/// out. Pages with no cached block have none.
pub fn page_vectors(items: &[Item], cache: &Cache) -> BTreeMap<usize, Vec<f32>> {
    let mut sums: BTreeMap<usize, (Vec<f32>, usize)> = BTreeMap::new();
    for item in items {
        let Some(v) = cache.vectors.get(&item.hash) else {
            continue;
        };
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm == 0.0 {
            continue;
        }
        let (sum, n) = sums
            .entry(item.page)
            .or_insert_with(|| (vec![0.0; v.len()], 0));
        if sum.len() != v.len() {
            continue;
        }
        for (s, x) in sum.iter_mut().zip(v) {
            *s += x / norm;
        }
        *n += 1;
    }
    sums.into_iter()
        .map(|(page, (sum, n))| (page, sum.into_iter().map(|s| s / n as f32).collect()))
        .collect()
}

/// The pages most similar to page `page` (by `page_vectors`), best first,
/// as `(cosine, page index)`, at most `limit`, only positive scores.
/// Empty when `page` has no cached block.
pub fn related_pages(
    page: usize,
    items: &[Item],
    cache: &Cache,
    limit: usize,
) -> Vec<(f32, usize)> {
    let vectors = page_vectors(items, cache);
    let Some(own) = vectors.get(&page) else {
        return Vec::new();
    };
    let mut scored: Vec<(f32, usize)> = vectors
        .iter()
        .filter(|(p, _)| **p != page)
        .map(|(p, v)| (cosine(own, v), *p))
        .filter(|(score, _)| *score > 0.0)
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored.truncate(limit);
    scored
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("notesec-sem-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn key(model: &str) -> CacheKey {
        CacheKey {
            provider: "local".into(),
            base: "http://localhost:1234/v1".into(),
            model: model.into(),
        }
    }

    #[test]
    fn items_skip_empty_blocks_and_hash_their_text() {
        let pages = vec![
            Page::from_markdown("A", false, "- one\n- \n- two\n"),
            Page::from_markdown("B", false, "- one\n"),
        ];
        let items = items(&pages);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].text, "A\none");
        assert_eq!((items[1].page, items[1].block), (0, 2));
        assert_ne!(
            items[0].hash, items[2].hash,
            "the title is part of the text"
        );
        assert_eq!(text_hash("abc"), text_hash("abc"));
        assert_eq!(text_hash(""), "cbf29ce484222325");
        let long = "x".repeat(5000);
        assert_eq!(embed_text("T", &long).chars().count(), MAX_EMBED_CHARS);
    }

    #[test]
    fn cache_round_trips_switches_keys_and_reports_missing() {
        let dir = temp_dir("cache");
        let pages = vec![Page::from_markdown("A", false, "- one\n- two\n- one\n")];
        let items = items(&pages);
        let mut cache = Cache::load(&dir);
        assert!(cache.use_key(&key("m")));
        assert_eq!(cache.missing(&items).len(), 2, "same text once");
        cache.vectors.insert(items[0].hash.clone(), vec![1.0, 0.0]);
        cache.vectors.insert("stale".into(), vec![0.0]);
        cache.prune(&items);
        assert!(!cache.vectors.contains_key("stale"));
        cache.save(&dir).unwrap();
        assert!(dir.join(".notesec/.gitignore").exists());
        let loaded = Cache::load(&dir);
        assert_eq!(loaded.vectors, cache.vectors);
        assert_eq!(loaded.missing(&items).len(), 1);
        let mut loaded = loaded;
        assert!(!loaded.use_key(&key("m")));
        assert!(
            loaded.use_key(&key("other")),
            "a new model drops the vectors"
        );
        assert!(loaded.vectors.is_empty());
        fs::write(Cache::path(&dir), "not json").unwrap();
        assert_eq!(Cache::load(&dir), Cache::default());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn cosine_ranks_blocks_and_pages_by_their_best_block() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert_eq!(cosine(&[1.0, 0.0], &[0.0, 1.0]), 0.0);
        assert_eq!(cosine(&[1.0], &[1.0, 2.0]), 0.0);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 2.0]), 0.0);
        let pages = vec![
            Page::from_markdown("A", false, "- far\n- near\n"),
            Page::from_markdown("B", false, "- middle\n"),
        ];
        let items = items(&pages);
        let mut cache = Cache::default();
        for (item, v) in items.iter().zip([[0.0, 1.0], [1.0, 0.1], [1.0, 1.0]]) {
            cache.vectors.insert(item.hash.clone(), v.to_vec());
        }
        let query = [1.0, 0.0];
        let blocks: Vec<usize> = rank_blocks(&query, &items, &cache)
            .into_iter()
            .map(|(_, i)| i)
            .collect();
        assert_eq!(blocks, vec![1, 2, 0]);
        let pages: Vec<usize> = rank_pages(&query, &items, &cache, 10)
            .into_iter()
            .map(|(_, i)| i)
            .collect();
        assert_eq!(pages, vec![1, 2], "page A by its best block, once");
        assert_eq!(rank_pages(&query, &items, &cache, 1).len(), 1);
    }

    #[test]
    fn related_pages_compare_mean_page_vectors() {
        let pages = vec![
            Page::from_markdown("A", false, "- x\n- y\n"),
            Page::from_markdown("B", false, "- near\n"),
            Page::from_markdown("C", false, "- far\n"),
            Page::from_markdown("D", false, "- unindexed\n"),
        ];
        let items = items(&pages);
        let mut cache = Cache::default();
        let set = |cache: &mut Cache, i: usize, v: Vec<f32>| {
            cache.vectors.insert(items[i].hash.clone(), v);
        };
        // A's blocks average to (1, 1, 0) (each unit vector counts once,
        // however long it is).
        set(&mut cache, 0, vec![2.0, 0.0, 0.0]);
        set(&mut cache, 1, vec![0.0, 1.0, 0.0]);
        set(&mut cache, 2, vec![1.0, 0.9, 0.0]);
        set(&mut cache, 3, vec![0.0, 0.0, 1.0]);
        let means = page_vectors(&items, &cache);
        assert_eq!(means[&0], vec![0.5, 0.5, 0.0]);
        assert!(!means.contains_key(&3));
        // C is orthogonal (score 0): left out. D has no vector.
        let related = related_pages(0, &items, &cache, 5);
        assert_eq!(related.iter().map(|r| r.1).collect::<Vec<_>>(), vec![1]);
        assert!(related[0].0 > 0.99);
        assert!(related_pages(3, &items, &cache, 5).is_empty());
    }
}
