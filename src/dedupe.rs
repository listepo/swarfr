//! The dedupe pass: equal content in different inodes becomes copy-on-write clones of one.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::time::{Duration, SystemTime};

use rayon::prelude::*;
use sha2::{Digest, Sha256};

use crate::engine::{Action, Pass, Replace, Share};
use crate::index::{Hash, HashIndex};
use crate::model::{Inode, Profile, Stamp};
use crate::sys::{self, COMPRESSED};

/// One APFS block. A smaller file occupies one block either way; nothing to gain.
pub const NAME: &str = "dedupe";
pub const DEFAULT_MIN_SIZE: u64 = 4096;
/// Younger files are likely to be rewritten by the next build, which un-shares them again.
pub const DEFAULT_MIN_AGE: Duration = Duration::from_secs(60 * 60);
const HASH_BUFFER_BYTES: usize = 1 << 20;

/// An inode that may join a content group: whether an earlier run already shared it, and how
/// this run would share it.
type Candidate<'a> = (&'a Inode, bool, Share);

pub struct Dedupe<'a> {
    pub min_size: u64,
    pub min_age: Duration,
    /// Where the filesystem cannot share blocks, share the inode instead. True for the cargo
    /// home's unpacked sources, which cargo never rewrites in place; for a target dir only when
    /// the user asks with `--link-artifacts`, because rustc truncates its outputs and would
    /// rewrite every name pointing at the same inode. See [`Share::Link`]. A `Cell`, so a run
    /// sets it per group, from the adapter whose build dirs the group holds.
    pub link_fallback: Cell<bool>,
    index: &'a RefCell<HashIndex>,
    hashed: Cell<usize>,
}

impl<'a> Dedupe<'a> {
    /// The index is the caller's: the compress pass reads it too, and the caller saves it.
    pub fn new(index: &'a RefCell<HashIndex>) -> Self {
        Self {
            min_size: DEFAULT_MIN_SIZE,
            min_age: DEFAULT_MIN_AGE,
            link_fallback: Cell::new(false),
            index,
            hashed: Cell::new(0),
        }
    }

    /// Files read and hashed so far; everything else came from the index or was never needed.
    pub fn hashed(&self) -> usize {
        self.hashed.get()
    }

    /// How this profile's files may be shared, or `None` when they may not be. A filesystem
    /// with copy-on-write always answers [`Share::Clone`]; without it the only way to share is
    /// one inode under both names, which is [`Self::link_fallback`]'s question to answer.
    ///
    /// Not one file is read to decide this.
    fn share(&self, profile: &Profile) -> Option<Share> {
        if sys::caps(&profile.dir).clone {
            Some(Share::Clone)
        } else {
            self.link_fallback.get().then_some(Share::Link)
        }
    }

    /// Also the rule for a source: it must not be rewritten under our feet either.
    fn eligible(&self, inode: &Inode, now: SystemTime) -> bool {
        inode.stamp.size >= self.min_size
            && inode.nlink == inode.paths.len() as u64
            && inode.flags & !COMPRESSED == 0
            && now
                .duration_since(inode.stamp.mtime)
                .is_ok_and(|age| age >= self.min_age)
    }
}

impl Pass for Dedupe<'_> {
    fn name(&self) -> &'static str {
        NAME
    }

    fn plan(&self, profiles: &[Profile]) -> Vec<Action> {
        let now = SystemTime::now();
        // A file whose size is unique on its device has no twin: never read it.
        let mut by_size: HashMap<(u64, u64), Vec<(&Inode, Share)>> = HashMap::new();
        let sharing = profiles
            .iter()
            .filter_map(|profile| self.share(profile).map(|how| (profile, how)));
        for (profile, how) in sharing {
            for inode in &profile.inodes {
                if self.eligible(inode, now) {
                    let key = (inode.stamp.dev, inode.stamp.size);
                    by_size.entry(key).or_default().push((inode, how));
                }
            }
        }
        let candidates: Vec<(&Inode, Share)> = by_size
            .into_values()
            .filter(|bucket| bucket.len() > 1)
            .flatten()
            .collect();

        let mut index = self.index.borrow_mut();
        let unknown: Vec<&Inode> = candidates
            .iter()
            .map(|(inode, _)| *inode)
            .filter(|inode| index.get(&inode.stamp).is_none())
            .collect();
        let computed: Vec<(&Inode, io::Result<Hash>)> = unknown
            .par_iter()
            .map(|inode| (*inode, hash_file(&inode.paths[0])))
            .collect();
        self.hashed.set(self.hashed.get() + computed.len());
        for (inode, hash) in computed {
            // An unreadable file simply stays out of every group.
            if let Ok(hash) = hash {
                index.put(&inode.stamp, hash, false);
            }
        }

        let mut by_content: HashMap<(u64, Hash), Vec<Candidate<'_>>> = HashMap::new();
        for (inode, how) in candidates {
            if let Some(entry) = index.get(&inode.stamp) {
                let key = (inode.stamp.dev, entry.hash);
                by_content
                    .entry(key)
                    .or_default()
                    .push((inode, entry.shared, how));
            }
        }

        let mut actions = Vec::new();
        for mut group in by_content.into_values() {
            group.sort_by(|a, b| canonical_order(a).cmp(&canonical_order(b)));
            let (canonical, _, _) = group[0];
            // Inodes already marked shared are clones from an earlier run: leave them.
            // ponytail: two shared clusters with the same content are never merged.
            for (member, shared, how) in &group[1..] {
                if !shared {
                    actions.push(Replace {
                        source: canonical.paths[0].clone(),
                        source_stamp: canonical.stamp.clone(),
                        member: (*member).clone(),
                        how: *how,
                    });
                }
            }
        }
        actions.sort_by(|a, b| a.member.paths.cmp(&b.member.paths));
        actions.into_iter().map(Action::Replace).collect()
    }

    fn replaced(&self, replace: &Replace, new: &Stamp) {
        let mut index = self.index.borrow_mut();
        if let Some(hash) = index.get(&replace.source_stamp).map(|entry| entry.hash) {
            index.mark_shared(&replace.source_stamp);
            index.remove(&replace.member.stamp);
            index.put(new, hash, true);
        }
    }

    fn rewritten(&self, old: &Stamp, new: &Stamp) {
        let mut index = self.index.borrow_mut();
        if let Some(hash) = index.get(old).map(|entry| entry.hash) {
            index.remove(old);
            index.put(new, hash, false);
        }
    }
}

/// Smallest first: an already shared inode, then a compressed one (its clones stay compressed,
/// which is how dedupe and compress add up instead of fighting), then the oldest.
fn canonical_order<'a>(entry: &Candidate<'a>) -> (bool, bool, SystemTime, &'a Path) {
    let (inode, shared, _) = *entry;
    let compressed = inode.flags & COMPRESSED != 0;
    (
        !shared,
        !compressed,
        inode.stamp.mtime,
        inode.paths[0].as_path(),
    )
}

fn hash_file(path: &Path) -> io::Result<Hash> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; HASH_BUFFER_BYTES];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(hasher.finalize().into());
        }
        hasher.update(&buffer[..read]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn inode(name: &str, flags: u32, mtime_secs: u64) -> Inode {
        Inode {
            stamp: Stamp {
                dev: 1,
                ino: u128::from(mtime_secs),
                size: DEFAULT_MIN_SIZE,
                mtime: SystemTime::UNIX_EPOCH + Duration::from_secs(mtime_secs),
            },
            mode: 0o644,
            flags,
            nlink: 1,
            allocated: DEFAULT_MIN_SIZE,
            paths: vec![PathBuf::from(name)],
        }
    }

    #[test]
    fn canonical_is_shared_then_compressed_then_oldest() {
        let (old, new) = (inode("old", 0, 1), inode("new", 0, 2));
        let compressed = inode("compressed", COMPRESSED, 3);
        let shared = inode("shared", 0, 4);
        let mut group = [
            (&new, false, Share::Clone),
            (&compressed, false, Share::Clone),
            (&shared, true, Share::Clone),
            (&old, false, Share::Clone),
        ];

        group.sort_by(|a, b| canonical_order(a).cmp(&canonical_order(b)));

        let names: Vec<_> = group
            .iter()
            .map(|(i, _, _)| i.paths[0].to_str().unwrap())
            .collect();
        assert_eq!(names, ["shared", "compressed", "old", "new"]);
    }
}
