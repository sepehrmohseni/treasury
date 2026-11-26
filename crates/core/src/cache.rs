//! The page cache: fetched rows live in memory up to a byte budget shared by every open result,
//! and the least recently used pages beyond it move to a spill file.
//!
//! None of the drivers' cursors can scroll backwards, so an evicted page cannot be fetched again
//! from the server; it is written to the result's spill file first. Spill files are unnamed
//! temporary files (removed by the OS when the result closes, even after a crash) and every page
//! in them is sealed with XChaCha20-Poly1305 under a key that exists only in this process's
//! memory, so rows never reach the disk in readable form.

use crate::error::{Error, Result};
use crate::exec::Row;
use chacha20poly1305::{AeadInOut, KeyInit, Tag, XChaCha20Poly1305, XNonce};
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use zeroize::Zeroizing;

/// The default memory budget for cached pages: 64 MB.
pub const DEFAULT_BUDGET: usize = 64 * 1024 * 1024;

const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;

/// Identifies a page: the result it belongs to, the result set within it, and its index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PageKey {
    /// The result (execution) the page belongs to.
    pub result: u64,
    /// Result set within the execution.
    pub set: u32,
    /// Page number within the result set.
    pub page: u32,
}

struct Entry {
    rows: Option<Arc<Vec<Row>>>,
    bytes: usize,
    spilled: Option<(u64, u32)>,
    tick: u64,
}

struct Spill {
    file: File,
    end: u64,
}

struct Inner {
    entries: HashMap<PageKey, Entry>,
    lru: BTreeMap<u64, PageKey>,
    spills: HashMap<u64, Spill>,
    used: usize,
    tick: u64,
}

/// The shared page cache.
pub struct PageStore {
    budget: usize,
    dir: PathBuf,
    cipher: XChaCha20Poly1305,
    inner: Mutex<Inner>,
}

/// Memory and disk use, for the status bar and the memory budget test.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StoreStats {
    /// Bytes of rows held in memory.
    pub memory_bytes: usize,
    /// Pages held in memory.
    pub memory_pages: usize,
    /// Bytes written to spill files.
    pub spilled_bytes: u64,
}

impl PageStore {
    /// A store holding at most `budget` bytes of rows in memory, spilling to files in `dir`.
    pub fn new(budget: usize, dir: PathBuf) -> Result<Arc<Self>> {
        let mut key = Zeroizing::new([0u8; 32]);
        getrandom::fill(key.as_mut_slice())
            .map_err(|e| Error::internal(format!("no randomness for the spill key: {e}")))?;
        let cipher = XChaCha20Poly1305::new((&*key).into());
        Ok(Arc::new(Self {
            budget,
            dir,
            cipher,
            inner: Mutex::new(Inner {
                entries: HashMap::new(),
                lru: BTreeMap::new(),
                spills: HashMap::new(),
                used: 0,
                tick: 0,
            }),
        }))
    }

    /// The memory budget in bytes.
    pub fn budget(&self) -> usize {
        self.budget
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Adds a full page. Pages are immutable once added.
    pub fn insert(&self, key: PageKey, rows: Vec<Row>) -> Result<()> {
        let bytes = rows_size(&rows);
        let mut inner = self.lock();
        inner.tick += 1;
        let tick = inner.tick;
        if let Some(old) = inner.entries.remove(&key) {
            inner.lru.remove(&old.tick);
            if old.rows.is_some() {
                inner.used -= old.bytes;
            }
        }
        inner.entries.insert(
            key,
            Entry {
                rows: Some(Arc::new(rows)),
                bytes,
                spilled: None,
                tick,
            },
        );
        inner.lru.insert(tick, key);
        inner.used += bytes;
        self.evict(&mut inner)
    }

    /// The rows of a page, read back from the spill file if it was evicted.
    pub fn get(&self, key: PageKey) -> Result<Arc<Vec<Row>>> {
        let mut inner = self.lock();
        inner.tick += 1;
        let tick = inner.tick;
        let (old_tick, cached, spilled) = match inner.entries.get(&key) {
            Some(e) => (e.tick, e.rows.clone(), e.spilled),
            None => return Err(Error::internal(format!("page {key:?} is not in the cache"))),
        };
        inner.lru.remove(&old_tick);
        inner.lru.insert(tick, key);
        if let Some(entry) = inner.entries.get_mut(&key) {
            entry.tick = tick;
        }
        if let Some(rows) = cached {
            return Ok(rows);
        }
        let (offset, len) =
            spilled.ok_or_else(|| Error::internal("an evicted page has no spill record"))?;
        let rows = Arc::new(self.read_spill(&mut inner, key, offset, len)?);
        let bytes = rows_size(&rows);
        if let Some(entry) = inner.entries.get_mut(&key) {
            entry.rows = Some(rows.clone());
            entry.bytes = bytes;
        }
        inner.used += bytes;
        self.evict(&mut inner)?;
        Ok(rows)
    }

    /// Forgets every page of `result` and deletes its spill file.
    pub fn release(&self, result: u64) {
        let mut inner = self.lock();
        let keys: Vec<PageKey> = inner
            .entries
            .keys()
            .filter(|k| k.result == result)
            .copied()
            .collect();
        for k in keys {
            if let Some(e) = inner.entries.remove(&k) {
                inner.lru.remove(&e.tick);
                if e.rows.is_some() {
                    inner.used -= e.bytes;
                }
            }
        }
        // Dropping the unnamed file removes it.
        inner.spills.remove(&result);
    }

    /// Current memory and disk use.
    pub fn stats(&self) -> StoreStats {
        let inner = self.lock();
        StoreStats {
            memory_bytes: inner.used,
            memory_pages: inner.entries.values().filter(|e| e.rows.is_some()).count(),
            spilled_bytes: inner.spills.values().map(|s| s.end).sum(),
        }
    }

    /// Moves least recently used pages out of memory until the budget holds. Only pages held in
    /// memory are in the LRU order; the newest one always stays, so a single page larger than
    /// the budget can still be read.
    fn evict(&self, inner: &mut Inner) -> Result<()> {
        while inner.used > self.budget && inner.lru.len() > 1 {
            let Some((&tick, &key)) = inner.lru.iter().next() else {
                break;
            };
            inner.lru.remove(&tick);
            let (rows, spilled, bytes) = match inner.entries.get(&key) {
                Some(e) => (e.rows.clone(), e.spilled, e.bytes),
                None => continue,
            };
            let Some(rows) = rows else { continue };
            let record = match spilled {
                Some(r) => r,
                None => self.write_spill(inner, key, &rows)?,
            };
            if let Some(entry) = inner.entries.get_mut(&key) {
                entry.rows = None;
                entry.spilled = Some(record);
            }
            inner.used -= bytes;
        }
        Ok(())
    }

    fn write_spill(&self, inner: &mut Inner, key: PageKey, rows: &[Row]) -> Result<(u64, u32)> {
        let mut plain = Zeroizing::new(
            rmp_serde::to_vec(rows)
                .map_err(|e| Error::internal(format!("could not encode a page: {e}")))?,
        );
        let mut nonce = [0u8; NONCE_LEN];
        getrandom::fill(&mut nonce)
            .map_err(|e| Error::internal(format!("no randomness for a spill nonce: {e}")))?;
        let aad = aad(key);
        let xnonce: &XNonce = (&nonce).into();
        let tag = self
            .cipher
            .encrypt_inout_detached(xnonce, &aad, plain.as_mut_slice().into())
            .map_err(|_| Error::internal("a page is too large to spill"))?;
        let tag: [u8; TAG_LEN] = tag.into();
        let spill = match inner.spills.entry(key.result) {
            std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
            std::collections::hash_map::Entry::Vacant(v) => {
                let file = tempfile::tempfile_in(&self.dir).map_err(|e| spill_error(&e))?;
                v.insert(Spill { file, end: 0 })
            }
        };
        let offset = spill.end;
        let len = NONCE_LEN + plain.len() + TAG_LEN;
        spill
            .file
            .seek(SeekFrom::Start(offset))
            .and_then(|_| spill.file.write_all(&nonce))
            .and_then(|_| spill.file.write_all(&plain))
            .and_then(|_| spill.file.write_all(&tag))
            .map_err(|e| spill_error(&e))?;
        let len32 =
            u32::try_from(len).map_err(|_| Error::internal("a page is too large to spill"))?;
        spill.end += u64::from(len32);
        Ok((offset, len32))
    }

    fn read_spill(
        &self,
        inner: &mut Inner,
        key: PageKey,
        offset: u64,
        len: u32,
    ) -> Result<Vec<Row>> {
        let spill = inner
            .spills
            .get_mut(&key.result)
            .ok_or_else(|| Error::internal("the spill file is gone"))?;
        let len = usize::try_from(len).map_err(|_| Error::internal("bad spill record"))?;
        if len < NONCE_LEN + TAG_LEN {
            return Err(Error::internal("bad spill record"));
        }
        let mut buf = Zeroizing::new(vec![0u8; len]);
        spill
            .file
            .seek(SeekFrom::Start(offset))
            .and_then(|_| spill.file.read_exact(&mut buf))
            .map_err(|e| spill_error(&e))?;
        let (nonce, rest) = buf.split_at_mut(NONCE_LEN);
        let (body, tag) = rest.split_at_mut(rest.len() - TAG_LEN);
        let tag = Tag::try_from(&*tag).map_err(|_| Error::internal("bad spill record"))?;
        let nonce: [u8; NONCE_LEN] = (&*nonce)
            .try_into()
            .map_err(|_| Error::internal("bad spill record"))?;
        let xnonce: &XNonce = (&nonce).into();
        self.cipher
            .decrypt_inout_detached(xnonce, &aad(key), body.into(), &tag)
            .map_err(|_| Error::internal("a spilled page failed its integrity check"))?;
        rmp_serde::from_slice(body)
            .map_err(|e| Error::internal(format!("could not decode a spilled page: {e}")))
    }
}

fn aad(key: PageKey) -> [u8; 16] {
    let mut a = [0u8; 16];
    a[..8].copy_from_slice(&key.result.to_le_bytes());
    a[8..12].copy_from_slice(&key.set.to_le_bytes());
    a[12..].copy_from_slice(&key.page.to_le_bytes());
    a
}

fn spill_error(e: &std::io::Error) -> Error {
    Error::new(
        crate::error::ErrorKind::Internal,
        format!(
            "Could not write fetched rows to the temporary folder: {e}. Free some disk space, or \
             lower the page cache budget's use by fetching fewer rows."
        ),
    )
}

/// The approximate memory a page of rows occupies.
pub fn rows_size(rows: &[Row]) -> usize {
    rows.iter()
        .map(|r| {
            std::mem::size_of::<Row>() + r.iter().map(crate::Value::approx_size).sum::<usize>()
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Value;

    fn page(n: usize, tag: &str) -> Vec<Row> {
        (0..n)
            .map(|i| {
                vec![
                    Value::I64(i as i64),
                    Value::Text(format!("{tag}-{i:04}-{}", "x".repeat(100))),
                ]
            })
            .collect()
    }

    fn key(page: u32) -> PageKey {
        PageKey {
            result: 1,
            set: 0,
            page,
        }
    }

    #[test]
    fn pages_round_trip_through_the_spill_file() {
        let dir = tempfile::tempdir().unwrap();
        let one = rows_size(&page(100, "p0"));
        let store = PageStore::new(one * 2 + one / 2, dir.path().into()).unwrap();
        for p in 0..10 {
            store.insert(key(p), page(100, &format!("p{p}"))).unwrap();
            assert!(store.stats().memory_bytes <= store.budget() + one);
        }
        let stats = store.stats();
        assert!(stats.memory_pages <= 3, "{stats:?}");
        assert!(stats.spilled_bytes > 0);
        for p in 0..10 {
            let rows = store.get(key(p)).unwrap();
            assert_eq!(rows.len(), 100);
            assert_eq!(
                rows[7][1],
                Value::Text(format!("p{p}-0007-{}", "x".repeat(100)))
            );
        }
        assert!(store.stats().memory_bytes <= store.budget() + one);
    }

    #[test]
    fn spilled_bytes_are_not_plaintext() {
        let dir = tempfile::tempdir().unwrap();
        let store = PageStore::new(1, dir.path().into()).unwrap();
        store
            .insert(
                key(0),
                vec![vec![Value::Text("needle-in-the-spill".into())]],
            )
            .unwrap();
        store
            .insert(key(1), vec![vec![Value::Text("second".into())]])
            .unwrap();
        let inner = store.lock();
        let spill = inner.spills.get(&1).unwrap();
        let mut f = spill.file.try_clone().unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        let mut all = Vec::new();
        f.read_to_end(&mut all).unwrap();
        assert!(!all.is_empty());
        assert!(!all.windows(6).any(|w| w == b"needle"));
    }

    #[test]
    fn release_forgets_a_result() {
        let dir = tempfile::tempdir().unwrap();
        let store = PageStore::new(DEFAULT_BUDGET, dir.path().into()).unwrap();
        store.insert(key(0), page(10, "a")).unwrap();
        store
            .insert(
                PageKey {
                    result: 2,
                    set: 0,
                    page: 0,
                },
                page(10, "b"),
            )
            .unwrap();
        store.release(1);
        assert!(store.get(key(0)).is_err());
        assert!(
            store
                .get(PageKey {
                    result: 2,
                    set: 0,
                    page: 0
                })
                .is_ok()
        );
        assert_eq!(store.stats().memory_pages, 1);
    }

    #[test]
    fn a_page_larger_than_the_budget_is_still_readable() {
        let dir = tempfile::tempdir().unwrap();
        let store = PageStore::new(10, dir.path().into()).unwrap();
        store.insert(key(0), page(50, "big")).unwrap();
        assert_eq!(store.get(key(0)).unwrap().len(), 50);
    }
}
