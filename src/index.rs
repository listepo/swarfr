//! Persistent content-hash cache, so a run after a build hashes only the new inodes.
//!
//! A cache and nothing more: a missing, truncated or foreign file reads as empty, and losing it
//! costs one full rehash plus one redundant round of cloning.
//!
//! Every entry remembers when a run last looked it up, and [`HashIndex::expire`] drops the ones
//! no run has asked about for a while: the inodes of targets that were deleted, rebuilt or are
//! no longer under any root. Without it the file only grows.

use std::cell::Cell;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::model::Stamp;

pub const HASH_BYTES: usize = 32;
pub type Hash = [u8; HASH_BYTES];

/// File format: magic, then fixed-size little-endian records. Bump the digit on any change; a
/// file with another magic reads as empty and is rewritten by the next save.
const MAGIC: &[u8] = b"DUNIDX03";
const U64_BYTES: usize = 8;
const U32_BYTES: usize = 4;
const U128_BYTES: usize = 16;
/// dev (u64), ino (u128), size, mtime seconds (u64 each), mtime nanoseconds (u32), hash,
/// shared (u8), last seen in seconds since the epoch (u64). `ino` is 128-bit so a ReFS file id
/// fits; on every other platform the high half is zero.
const RECORD_BYTES: usize = 4 * U64_BYTES + U128_BYTES + U32_BYTES + HASH_BYTES + 1;
const TEMP_EXTENSION: &str = "tmp";
const NANOS_PER_SEC: u32 = 1_000_000_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub size: u64,
    pub mtime: SystemTime,
    pub hash: Hash,
    /// This tool cloned the inode, or cloned from it: its blocks are already shared.
    /// The filesystem cannot be asked, so without the mark every run would clone it again.
    pub shared: bool,
}

#[derive(Clone, Debug)]
struct Slot {
    entry: Entry,
    /// Seconds since the epoch of the last lookup. A `Cell`, so a lookup through `&self` counts.
    seen: Cell<u64>,
}

/// Equal when the entries are: when each was last looked up is not part of the content.
impl PartialEq for Slot {
    fn eq(&self, other: &Self) -> bool {
        self.entry == other.entry
    }
}

impl Eq for Slot {}

/// Equal when the entries are: when each was loaded is not part of the content.
#[derive(Debug, Default)]
pub struct HashIndex {
    entries: HashMap<(u64, u128), Slot>,
    /// What a lookup stamps on an entry: the time the index was loaded.
    now: u64,
}

impl PartialEq for HashIndex {
    fn eq(&self, other: &Self) -> bool {
        self.entries == other.entries
    }
}

impl Eq for HashIndex {}

impl HashIndex {
    pub fn load(path: &Path) -> Self {
        Self::load_at(path, SystemTime::now())
    }

    /// As [`Self::load`], with lookups stamped as made at `now`.
    pub fn load_at(path: &Path, now: SystemTime) -> Self {
        let mut index = fs::read(path)
            .ok()
            .and_then(|bytes| Self::decode(&bytes))
            .unwrap_or_default();
        index.now = epoch_secs(now);
        index
    }

    /// The entry for this exact version of the inode; a rewritten file misses. A hit counts as
    /// the entry being seen.
    pub fn get(&self, stamp: &Stamp) -> Option<&Entry> {
        let slot = self
            .entries
            .get(&(stamp.dev, stamp.ino))
            .filter(|slot| slot.entry.size == stamp.size && slot.entry.mtime == stamp.mtime)?;
        slot.seen.set(self.now);
        Some(&slot.entry)
    }

    pub fn put(&mut self, stamp: &Stamp, hash: Hash, shared: bool) {
        let entry = Entry {
            size: stamp.size,
            mtime: stamp.mtime,
            hash,
            shared,
        };
        let seen = Cell::new(self.now);
        self.entries
            .insert((stamp.dev, stamp.ino), Slot { entry, seen });
    }

    pub fn mark_shared(&mut self, stamp: &Stamp) {
        if let Some(slot) = self.entries.get_mut(&(stamp.dev, stamp.ino))
            && slot.entry.size == stamp.size
            && slot.entry.mtime == stamp.mtime
        {
            slot.entry.shared = true;
            slot.seen.set(self.now);
        }
    }

    /// Drop every entry no lookup has hit for longer than `idle`. The cost of dropping one that
    /// is still wanted is one rehash of that file.
    pub fn expire(&mut self, idle: Duration) {
        let cutoff = self.now.saturating_sub(idle.as_secs());
        self.entries.retain(|_, slot| slot.seen.get() >= cutoff);
    }

    /// Forget an inode that no longer exists.
    pub fn remove(&mut self, stamp: &Stamp) {
        self.entries.remove(&(stamp.dev, stamp.ino));
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Temp file plus `rename`: a crash never leaves a half-written index.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temp = path.with_extension(TEMP_EXTENSION);
        fs::write(&temp, self.encode())?;
        fs::rename(&temp, path)
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(MAGIC.len() + self.entries.len() * RECORD_BYTES);
        out.extend_from_slice(MAGIC);
        for (&(dev, ino), Slot { entry, seen }) in &self.entries {
            // A pre-1970 mtime is not worth a format with signed seconds; just do not cache it.
            let Ok(mtime) = entry.mtime.duration_since(SystemTime::UNIX_EPOCH) else {
                continue;
            };
            out.extend_from_slice(&dev.to_le_bytes());
            out.extend_from_slice(&ino.to_le_bytes());
            for value in [entry.size, mtime.as_secs()] {
                out.extend_from_slice(&value.to_le_bytes());
            }
            out.extend_from_slice(&mtime.subsec_nanos().to_le_bytes());
            out.extend_from_slice(&entry.hash);
            out.push(u8::from(entry.shared));
            out.extend_from_slice(&seen.get().to_le_bytes());
        }
        out
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let records = bytes.strip_prefix(MAGIC)?;
        let (records, rest) = records.as_chunks::<RECORD_BYTES>();
        if !rest.is_empty() {
            return None;
        }
        let mut entries = HashMap::new();
        for record in records {
            let mut record: &[u8] = record;
            let mut take = |n: usize| {
                let (head, tail) = record.split_at(n);
                record = tail;
                head
            };
            let dev = u64::from_le_bytes(take(U64_BYTES).try_into().unwrap());
            let ino = u128::from_le_bytes(take(U128_BYTES).try_into().unwrap());
            let size = u64::from_le_bytes(take(U64_BYTES).try_into().unwrap());
            let secs = u64::from_le_bytes(take(U64_BYTES).try_into().unwrap());
            let nanos = u32::from_le_bytes(take(U32_BYTES).try_into().unwrap());
            let hash: Hash = take(HASH_BYTES).try_into().unwrap();
            let shared = take(1)[0] != 0;
            let seen = Cell::new(u64::from_le_bytes(take(U64_BYTES).try_into().unwrap()));
            if nanos >= NANOS_PER_SEC {
                return None; // `Duration::new` would carry, and panic on overflow
            }
            let mtime = SystemTime::UNIX_EPOCH.checked_add(Duration::new(secs, nanos))?;
            let entry = Entry {
                size,
                mtime,
                hash,
                shared,
            };
            entries.insert((dev, ino), Slot { entry, seen });
        }
        Some(Self { entries, now: 0 })
    }
}

/// Seconds since the epoch; a clock before 1970 counts as the epoch.
fn epoch_secs(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    fn stamp(ino: u64) -> Stamp {
        Stamp {
            dev: 1,
            ino: u128::from(ino),
            size: 4096,
            mtime: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000),
        }
    }

    #[test]
    fn entries_no_run_looks_up_go_and_the_ones_it_does_stay() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("hashes.bin");
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000);
        let (kept, gone) = (stamp(1), stamp(2));
        let mut index = HashIndex::load_at(&path, start);
        index.put(&kept, [1; HASH_BYTES], false);
        index.put(&gone, [2; HASH_BYTES], false);
        index.save(&path).unwrap();

        // Twenty days on, a run looks up one of them; nothing is old enough yet.
        let mut index = HashIndex::load_at(&path, start + 20 * DAY);
        assert!(index.get(&kept).is_some());
        index.expire(30 * DAY);
        assert_eq!(index.len(), 2);
        index.save(&path).unwrap();

        // Forty days on, the one nobody asked about is thirty-plus days idle; the other is not.
        let mut index = HashIndex::load_at(&path, start + 40 * DAY);
        index.expire(30 * DAY);
        assert!(index.get(&kept).is_some());
        assert!(index.get(&gone).is_none());
        assert_eq!(index.len(), 1);
    }

    #[test]
    fn an_index_of_the_previous_format_reads_as_empty_and_is_rewritten() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("hashes.bin");
        // `DUNIDX02`: the previous record, with a 64-bit inode. A wider id bumped the magic.
        let mut old = b"DUNIDX02".to_vec();
        old.resize(old.len() + 4 * U64_BYTES + U32_BYTES + HASH_BYTES + 1, 0);
        fs::write(&path, old).unwrap();

        let mut index = HashIndex::load(&path);
        assert!(index.is_empty());
        index.put(&stamp(1), [1; HASH_BYTES], false);
        index.save(&path).unwrap();

        assert!(fs::read(&path).unwrap().starts_with(MAGIC));
        assert_eq!(HashIndex::load(&path).len(), 1);
    }
}
