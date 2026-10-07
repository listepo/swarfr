//! `lock → scan → plan → apply → report`, and the safety invariants of `DESIGN.md`.
//!
//! The engine never decides *what* to replace, that is a pass's job, and it does not compare
//! contents. It guarantees *how*: under cargo's own lock, whole hardlink groups only, never over
//! a file that changed since the scan, and through a temp file plus `rename`. A pass that
//! rewrites content (compression) only ever gets private copies to work on.

use std::cell::Cell;
use std::fs::{self, File, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use crate::eco::{Ecosystem, Guard};
use crate::model::{self, Inode, Profile, Stamp, TMP_PREFIX};
use crate::sys::{self, COMPRESSED};

/// Copies handed to [`Pass::compress`] at once: bounds the work a crash throws away and the
/// time between checking a group and swapping its copy in.
const COMPRESS_BATCH: usize = 256;
/// Rounds one run makes at most with [`Options::until_settled`]: a pass that keeps finding
/// work must not keep the locks forever.
const MAX_ROUNDS: usize = 8;
/// Files of a [`Guard::Quiet`] unit younger than this are left out of the model: with no lock,
/// a file the build wrote an hour ago may be one it is still writing. No pass setting lowers it.
pub const QUIET_MIN_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// The same floor for a [`Guard::Immutable`] store: an entry is never rewritten, only written
/// once, and an hour keeps the passes off one still being written.
pub const IMMUTABLE_MIN_AGE: Duration = Duration::from_secs(60 * 60);

/// How a replacement gets its bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Share {
    /// A copy-on-write clone: its own inode and its own metadata, so a rewrite of either name
    /// touches only that name. The only safe choice for anything a build rewrites.
    Clone,
    /// A hardlink: one inode under every name. Cheap everywhere, including filesystems with no
    /// copy-on-write at all, and dangerous for exactly one reason — rustc opens its outputs with
    /// truncate, so a rebuild rewrites the inode in place and every other name with it. The
    /// pass decides where that is acceptable; the engine only refuses to change a file's mode
    /// on the way.
    Link,
}

/// Replace every path of `member` with `source`, shared the way `how` says.
#[derive(Clone, Debug)]
pub struct Replace {
    pub source: PathBuf,
    pub source_stamp: Stamp,
    pub member: Inode,
    pub how: Share,
}

#[derive(Clone, Debug)]
pub enum Action {
    Replace(Replace),
    /// Replace every path of the inode with a compressed copy of itself.
    Compress(Inode),
    /// Lossy: delete a profile dir whose lock we hold, the way `cargo clean` would, or a dir
    /// inside one, such as `incremental/`. `reason` is for the report, dry run included.
    Remove {
        dir: PathBuf,
        reason: String,
    },
    /// Lossy: delete `dir`, which lies inside `target` but outside its profile dirs — the whole
    /// target itself (`doc/` and `CACHEDIR.TAG` included) or something beside them, such as
    /// `doc/` alone. Applied only when a profile dir we hold the lock for is inside `target` and
    /// no dir reported busy is: what guards such a removal is the target's own build locks.
    /// `bytes` is the caller's own measurement, because the engine only scans profile dirs.
    RemoveTarget {
        target: PathBuf,
        dir: PathBuf,
        reason: String,
        bytes: u64,
    },
}

pub trait Pass {
    fn name(&self) -> &'static str;
    /// Lossy passes delete rebuildable data and run only when named in [`Options::lossy`].
    fn lossy(&self) -> bool {
        false
    }
    /// Sees only profiles whose lock is held. May read files, must not change anything.
    fn plan(&self, profiles: &[Profile]) -> Vec<Action>;
    /// Called after `replace` was applied; `new` is the stamp of the inode now at its paths.
    fn replaced(&self, _replace: &Replace, _new: &Stamp) {}
    /// For a pass that plans [`Action::Compress`]: compress these files where they are. They
    /// are private copies with one link each; one that comes back without the compressed flag
    /// is thrown away and its group stays as it was.
    fn compress(&self, _copies: &[PathBuf]) {}
    /// Told to every pass, not only the planning one: the content of inode `old` now lives,
    /// byte for byte, in the unshared inode `new`.
    fn rewritten(&self, _old: &Stamp, _new: &Stamp) {}
}

#[derive(Debug, Default)]
pub struct Options {
    pub dry_run: bool,
    pub lossy: Vec<String>,
    /// Run the passes again while a round applies anything; a dry run is always one round.
    pub until_settled: bool,
}

/// Why a planned group was left alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Skip {
    /// A path lies outside the profile dirs we hold the lock for.
    Unlocked,
    /// The inode has links outside the profile dir; replacing ours would split the group.
    ForeignLinks,
    /// Flags other than `COMPRESSED` (immutable, append-only, …) are not ours to drop.
    Flags,
    CrossDevice,
    SameInode,
    SizeMismatch,
    /// A hardlink would put one mode on both names, and these two do not agree on one.
    ModeMismatch,
    /// Size, mtime or inode differs from the scan: cargo or rustc got there first.
    Changed,
    /// The backend left the copy uncompressed: not worth it, not supported, or an error there.
    NotCompressed,
    /// Lossy, in a unit without a lock where a check said "maybe": a young file, or a build
    /// tool nobody could look for.
    Unsure,
    /// The file is in use: running, or open without sharing on Windows. Its unit counts as busy.
    Busy,
    Failed(io::ErrorKind),
}

#[derive(Debug, Default)]
pub struct PassReport {
    pub name: &'static str,
    pub planned: usize,
    pub planned_bytes: u64,
    pub applied: usize,
    /// Replaced inodes: their allocated bytes, an upper bound (a clone shares, it is not
    /// free). Compressed inodes: allocated bytes before minus after.
    pub freed_bytes: u64,
    pub skipped: Vec<(PathBuf, Skip)>,
    /// Every planned removal with its reason; on a dry run nothing of it happened. Planned
    /// minus skipped is what is gone.
    pub removals: Vec<(PathBuf, String)>,
}

impl PassReport {
    /// A later round: it adds what it applied, and what it skipped or removed for the first
    /// time. Planned counts only those, since a group skipped for good is planned every round.
    fn absorb(&mut self, round: PassReport) {
        let new_skips: Vec<_> = round
            .skipped
            .into_iter()
            .filter(|(path, _)| !self.skipped.iter().any(|(seen, _)| seen == path))
            .collect();
        self.planned += round.applied + new_skips.len();
        self.planned_bytes += round.freed_bytes;
        self.applied += round.applied;
        self.freed_bytes += round.freed_bytes;
        self.skipped.extend(new_skips);
        for removal in round.removals {
            if !self.removals.iter().any(|(seen, _)| *seen == removal.0) {
                self.removals.push(removal);
            }
        }
    }

    fn skip(&mut self, member: &Inode, skip: Skip) {
        let path = member.paths.first().cloned().unwrap_or_default();
        self.skipped.push((path, skip));
    }
}

#[derive(Debug, Default)]
pub struct Report {
    /// Units a build was seen in: skipped because it held their lock or its tool ran there, or
    /// worked on until one of their files was found in use.
    pub busy: Vec<PathBuf>,
    /// Units worked on without a lock: [`Guard::Quiet`] and [`Guard::Immutable`] ones.
    pub quiet: Vec<PathBuf>,
    pub temps_removed: usize,
    pub passes: Vec<PassReport>,
    /// Set when the run let go before its plan was done; the rest was never started.
    pub interrupted: Option<Interrupted>,
    /// Rounds of every pass that ran; more than one only with [`Options::until_settled`].
    pub rounds: usize,
}

impl Report {
    /// Adds a round's pass reports to the ones before it, pass by pass.
    fn absorb(&mut self, round: Vec<PassReport>) {
        if self.passes.is_empty() {
            self.passes = round;
            return;
        }
        for (total, pass) in self.passes.iter_mut().zip(round) {
            total.absorb(pass);
        }
    }
}

/// Why a run let go of its locks early.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interrupted {
    /// The caller raised its stop flag.
    Stopped,
    /// The locks were held for as long as the caller allowed.
    OutOfBudget,
}

/// When a run has to let go, checked between two actions and between two compress batches:
/// every action is a whole replacement or removal, so what a run leaves behind is old or new,
/// never half of either.
#[derive(Clone, Copy, Debug, Default)]
pub struct Interrupt<'a> {
    pub stop: Option<&'a AtomicBool>,
    pub deadline: Option<Instant>,
}

impl Interrupt<'_> {
    fn due(&self) -> Option<Interrupted> {
        if self.stop.is_some_and(|stop| stop.load(Ordering::Relaxed)) {
            Some(Interrupted::Stopped)
        } else if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            Some(Interrupted::OutOfBudget)
        } else {
            None
        }
    }
}

/// Delete `dir` whole and account for it, unless the caller's guard already refused it.
/// The locks of the profile dirs inside stay open until the run ends; their files are gone,
/// which is exactly what `cargo clean` leaves behind as well.
fn remove(
    dir: PathBuf,
    refused: Option<Skip>,
    bytes: u64,
    locked: &mut Vec<PathBuf>,
    pass_report: &mut PassReport,
) {
    let skip = refused.or_else(|| fs::remove_dir_all(&dir).err().map(|e| failed(&e)));
    match skip {
        None => {
            pass_report.applied += 1;
            pass_report.freed_bytes += bytes;
            locked.retain(|kept| !kept.starts_with(&dir));
        }
        Some(skip) => pass_report.skipped.push((dir, skip)),
    }
}

/// The build's own lock on a unit, held exclusively until dropped.
pub struct ProfileLock {
    _file: File,
}

impl ProfileLock {
    /// The lock `guard` names. `None` when a build holds it, and for [`Guard::Quiet`] and
    /// [`Guard::Immutable`], which have none to take. A missing [`Guard::Lock`] file is an error:
    /// not a unit. A missing [`Guard::Shared`] one is created, as the build tool creates it: it
    /// lives outside the units, where a temp dir cleaner may have taken it. [`Guard::Held`] is not
    /// implemented and refused.
    pub fn try_guard(guard: &Guard) -> io::Result<Option<Self>> {
        match guard {
            Guard::Lock(file) => Self::try_lock_file(file, false),
            Guard::Shared(file) => Self::try_lock_file(file, true),
            Guard::Quiet | Guard::Immutable => Ok(None),
            Guard::Held => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("{guard:?} is not implemented"),
            )),
        }
    }

    fn try_lock_file(file: &Path, create: bool) -> io::Result<Option<Self>> {
        let file = File::options()
            .read(true)
            .write(true)
            .create(create)
            .truncate(false)
            .open(file)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(e),
        }
    }
}

/// Every pass over `profile_dirs`, the units of `eco`, each under the guard `eco` names for it.
/// A unit whose guard a build holds is left out of the run; units under one shared guard are
/// all in or all out.
pub fn run(
    profile_dirs: &[PathBuf],
    passes: &[&dyn Pass],
    opts: &Options,
    eco: &dyn Ecosystem,
) -> io::Result<Report> {
    run_with(profile_dirs, passes, opts, eco, Interrupt::default())
}

/// [`run`], letting go early when `interrupt` says so.
pub fn run_with(
    profile_dirs: &[PathBuf],
    passes: &[&dyn Pass],
    opts: &Options,
    eco: &dyn Ecosystem,
    interrupt: Interrupt<'_>,
) -> io::Result<Report> {
    // Sorted order, so two concurrent runs cannot take the same locks in opposite order.
    let mut dirs = profile_dirs.to_vec();
    dirs.sort();
    dirs.dedup();

    let mut report = Report::default();
    let mut locks = Vec::new();
    let mut locked = Vec::new();
    // A shared guard is tried once, in the order its first unit comes.
    let mut shared: Vec<(Guard, bool)> = Vec::new();
    // Quiet units where a check could not say "no build here": lossy passes leave them alone.
    let mut unsure: Vec<PathBuf> = Vec::new();
    // Units without a lock, with the age their files need to be in the model.
    let mut floors: Vec<(PathBuf, Duration)> = Vec::new();
    for dir in dirs {
        let guard = eco.guard(&dir);
        let held = match &guard {
            Guard::Quiet => match sys::tool_running(&dir, eco.tools()) {
                Some(true) => false,
                found => {
                    if found.is_none() {
                        unsure.push(dir.clone());
                    }
                    floors.push((dir.clone(), QUIET_MIN_AGE));
                    report.quiet.push(dir.clone());
                    true
                }
            },
            // Nothing a lossy pass could want is here: the store's own tool evicts from it.
            Guard::Immutable => {
                unsure.push(dir.clone());
                floors.push((dir.clone(), IMMUTABLE_MIN_AGE));
                report.quiet.push(dir.clone());
                true
            }
            Guard::Shared(_) => match shared.iter().find(|(seen, _)| *seen == guard) {
                Some(&(_, held)) => held,
                None => {
                    let lock = ProfileLock::try_guard(&guard)?;
                    let held = lock.is_some();
                    locks.extend(lock);
                    shared.push((guard, held));
                    held
                }
            },
            _ => match ProfileLock::try_guard(&guard)? {
                Some(lock) => {
                    locks.push(lock);
                    true
                }
                None => false,
            },
        };
        if held {
            locked.push(dir);
        } else {
            report.busy.push(dir);
        }
    }

    let mut profiles = scan_all(&locked, eco, &floors, &mut unsure)?;
    if !opts.dry_run {
        for temp in profiles.iter().flat_map(|p| &p.stale_temps) {
            fs::remove_file(temp)?;
            report.temps_removed += 1;
        }
    }

    // Passes feed each other — dedupe's clones are files compress has not looked at — so a
    // round can leave work for the next one. Every round runs under the locks taken above.
    loop {
        report.rounds += 1;
        let mut round = Vec::new();
        let mut progressed = false;
        for pass in passes {
            if pass.lossy() && !opts.lossy.iter().any(|name| name == pass.name()) {
                continue;
            }
            let mut pass_report = PassReport {
                name: pass.name(),
                ..PassReport::default()
            };
            let mut removal_tried = false;
            let mut to_compress = Vec::new();
            for action in pass.plan(&profiles) {
                if let Some(why) = interrupt.due() {
                    report.interrupted = Some(why);
                    break;
                }
                let bytes = match &action {
                    Action::Replace(replace) => replace.member.allocated,
                    Action::Compress(inode) => inode.allocated,
                    Action::Remove { dir, reason } => {
                        pass_report.removals.push((dir.clone(), reason.clone()));
                        profiles
                            .iter()
                            .filter(|profile| dir.starts_with(&profile.dir))
                            .flat_map(|profile| &profile.inodes)
                            // A link from outside survives the removal, so nothing is freed by it.
                            .filter(|inode| inode.paths.iter().all(|path| path.starts_with(dir)))
                            .map(|inode| inode.allocated)
                            .sum()
                    }
                    Action::RemoveTarget {
                        dir, reason, bytes, ..
                    } => {
                        pass_report.removals.push((dir.clone(), reason.clone()));
                        *bytes
                    }
                };
                pass_report.planned += 1;
                pass_report.planned_bytes += bytes;
                if opts.dry_run {
                    continue;
                }
                match action {
                    Action::Compress(inode) => to_compress.push(inode),
                    Action::Remove { dir, .. } => {
                        removal_tried = true;
                        // A locked profile dir or something inside it, never one around it.
                        let refused = if !locked.iter().any(|held| dir.starts_with(held)) {
                            Some(Skip::Unlocked)
                        } else if unsure.iter().any(|unit| dir.starts_with(unit)) {
                            Some(Skip::Unsure)
                        } else {
                            None
                        };
                        remove(dir, refused, bytes, &mut locked, &mut pass_report);
                    }
                    Action::RemoveTarget { target, dir, .. } => {
                        removal_tried = true;
                        // Ours only if the target holds still: we have a lock inside it, nothing in
                        // it is being built, and what goes is inside it.
                        let holds_lock = locked.iter().any(|held| held.starts_with(&target));
                        let building = report.busy.iter().any(|busy| busy.starts_with(&target));
                        let refused = if !holds_lock || building || !dir.starts_with(&target) {
                            Some(Skip::Unlocked)
                        } else if unsure.iter().any(|unit| unit.starts_with(&target)) {
                            Some(Skip::Unsure)
                        } else {
                            None
                        };
                        remove(dir, refused, bytes, &mut locked, &mut pass_report);
                    }
                    Action::Replace(replace) => match apply_replace(&replace, &locked) {
                        None => {
                            pass_report.applied += 1;
                            pass_report.freed_bytes += replace.member.allocated;
                            // Under the lock nobody else can have touched the new inode yet.
                            pass.replaced(&replace, &Stamp::read(&replace.member.paths[0])?);
                        }
                        Some(skip) => pass_report.skip(&replace.member, skip),
                    },
                }
            }
            for batch in to_compress.chunks(COMPRESS_BATCH) {
                if let Some(why) = interrupt.due() {
                    report.interrupted = Some(why);
                    break;
                }
                apply_compress(
                    batch,
                    &locked,
                    *pass,
                    passes,
                    eco.lifts_read_only_dirs(),
                    &mut pass_report,
                )?;
            }
            if report.interrupted.is_some() {
                round.push(pass_report);
                break;
            }
            note_busy(&pass_report, &locked, &mut report.busy);
            // A removal that failed half way has changed the dir too.
            if pass_report.applied > 0 || removal_tried {
                // ponytail: full rescan so the next pass sees the new inodes; patch the model in
                // place if scan time ever shows up in the benchmarks.
                profiles = scan_all(&locked, eco, &floors, &mut unsure)?;
            }
            progressed |= pass_report.applied > 0;
            round.push(pass_report);
        }
        report.absorb(round);
        // "Applied nothing", not "planned nothing": a group skipped for good is planned again
        // by every round.
        let again = opts.until_settled
            && !opts.dry_run
            && progressed
            && report.interrupted.is_none()
            && report.rounds < MAX_ROUNDS;
        if !again {
            break;
        }
    }
    drop(locks);
    Ok(report)
}

/// Scans `dirs`. In a unit with a floor, files younger than it are left out, and a unit that had
/// any becomes `unsure`.
fn scan_all(
    dirs: &[PathBuf],
    eco: &dyn Ecosystem,
    floors: &[(PathBuf, Duration)],
    unsure: &mut Vec<PathBuf>,
) -> io::Result<Vec<Profile>> {
    let now = SystemTime::now();
    let mut profiles = Vec::with_capacity(dirs.len());
    for dir in dirs {
        let mut profile = model::scan(dir, eco)?;
        if let Some((_, floor)) = floors.iter().find(|(unit, _)| unit == dir) {
            let young = |inode: &Inode| {
                now.duration_since(inode.stamp.mtime)
                    .map_or(true, |age| age < *floor)
            };
            let before = profile.inodes.len();
            profile.inodes.retain(|inode| !young(inode));
            if profile.inodes.len() < before && !unsure.contains(dir) {
                unsure.push(dir.clone());
            }
        }
        profiles.push(profile);
    }
    Ok(profiles)
}

/// What an I/O error on one file makes of it: busy when a build has the file in use, which is
/// what a build without a lock looks like from outside.
fn failed(e: &io::Error) -> Skip {
    let busy = matches!(
        e.kind(),
        io::ErrorKind::ExecutableFileBusy | io::ErrorKind::ResourceBusy
    ) || (cfg!(windows)
        && matches!(
            e.raw_os_error(),
            Some(SHARING_VIOLATION | LOCK_VIOLATION | ACCESS_DENIED)
        ));
    if busy {
        Skip::Busy
    } else {
        Skip::Failed(e.kind())
    }
}

/// `ERROR_SHARING_VIOLATION` and `ERROR_LOCK_VIOLATION`: another process has the file open
/// without sharing, or a range of it locked. A `rename` over a file opened without
/// `FILE_SHARE_DELETE` comes back as `ERROR_ACCESS_DENIED` on current Windows, not as a
/// sharing violation — measured, the two are the same situation.
const SHARING_VIOLATION: i32 = 32;
const LOCK_VIOLATION: i32 = 33;
const ACCESS_DENIED: i32 = 5;

/// Adds the unit of every file the pass found busy to `busy`, once.
fn note_busy(pass_report: &PassReport, locked: &[PathBuf], busy: &mut Vec<PathBuf>) {
    let found = pass_report
        .skipped
        .iter()
        .filter(|(_, skip)| *skip == Skip::Busy)
        .flat_map(|(path, _)| {
            locked
                .iter()
                .filter(move |unit| path.starts_with(unit) || unit.starts_with(path))
        });
    for unit in found {
        if !busy.contains(unit) {
            busy.push(unit.clone());
        }
    }
}

fn apply_replace(replace: &Replace, locked: &[PathBuf]) -> Option<Skip> {
    try_replace(replace, locked).unwrap_or_else(|e| Some(failed(&e)))
}

fn try_replace(replace: &Replace, locked: &[PathBuf]) -> io::Result<Option<Skip>> {
    let Replace {
        source,
        source_stamp,
        member,
        how: _,
    } = replace;
    if !is_locked(source, locked) {
        return Ok(Some(Skip::Unlocked));
    }
    if let Some(skip) = check_group(member, locked)? {
        return Ok(Some(skip));
    }
    if source_stamp.dev != member.stamp.dev {
        return Ok(Some(Skip::CrossDevice));
    }
    if source_stamp.ino == member.stamp.ino {
        return Ok(Some(Skip::SameInode));
    }
    if source_stamp.size != member.stamp.size {
        return Ok(Some(Skip::SizeMismatch));
    }
    if Stamp::read(source)? != *source_stamp {
        return Ok(Some(Skip::Changed));
    }

    let temp = sibling_temp(&member.paths[0]);
    // The source of a clone is checked once more after the copy: a build without a lock may
    // have written it while it was copied, and those bytes must not land under the member's
    // names.
    let moved = Cell::new(false);
    let held_still = || {
        let still = Stamp::read(source)? == *source_stamp;
        moved.set(!still);
        if still {
            Ok(())
        } else {
            Err(io::Error::other("the source changed while it was copied"))
        }
    };
    let swapped = match replace.how {
        Share::Clone => swap_in(&temp, member, || {
            clone_as(source, &temp, member).and_then(|()| held_still())
        }),
        Share::Link => {
            // One inode under both names means one mode for both: linking files whose
            // permissions differ would quietly change the other name's.
            if sys::mode(&fs::symlink_metadata(source)?) != member.mode {
                return Ok(Some(Skip::ModeMismatch));
            }
            // No check after: a link is the source itself, and `link_as` may move its mtime.
            swap_in(&temp, member, || link_as(source, &temp, member))
        }
    };
    match swapped {
        Err(_) if moved.get() => Ok(Some(Skip::Changed)),
        other => other.map(|()| None),
    }
}

fn is_locked(path: &Path, locked: &[PathBuf]) -> bool {
    locked.iter().any(|dir| path.starts_with(dir))
}

/// What must hold for a group before any of its paths is replaced.
fn check_group(member: &Inode, locked: &[PathBuf]) -> io::Result<Option<Skip>> {
    if member.paths.is_empty() {
        return Ok(Some(Skip::Changed));
    }
    if !member.paths.iter().all(|path| is_locked(path, locked)) {
        return Ok(Some(Skip::Unlocked));
    }
    if member.nlink != member.paths.len() as u64 {
        return Ok(Some(Skip::ForeignLinks));
    }
    if member.flags & !COMPRESSED != 0 {
        return Ok(Some(Skip::Flags));
    }
    for path in &member.paths {
        if Stamp::read(path)? != member.stamp {
            return Ok(Some(Skip::Changed));
        }
    }
    Ok(None)
}

fn apply_compress(
    batch: &[Inode],
    locked: &[PathBuf],
    pass: &dyn Pass,
    passes: &[&dyn Pass],
    lift: bool,
    report: &mut PassReport,
) -> io::Result<()> {
    let lifted = if lift { lift_dirs(batch) } else { Vec::new() };
    let mut staged = Vec::new();
    for member in batch {
        match stage_copy(member, locked) {
            Ok(Ok(copy)) => staged.push((member, copy)),
            Ok(Err(skip)) => report.skip(member, skip),
            Err(e) => report.skip(member, failed(&e)),
        }
    }
    let copies: Vec<PathBuf> = staged.iter().map(|(_, copy)| copy.clone()).collect();
    pass.compress(&copies);
    for (member, copy) in staged {
        match finish_compress(member, &copy, locked) {
            Ok(Ok(new)) => {
                report.applied += 1;
                report.freed_bytes += member.allocated.saturating_sub(new.allocated);
                for pass in passes {
                    pass.rewritten(&member.stamp, &new.stamp);
                }
            }
            Ok(Err(skip)) => report.skip(member, skip),
            Err(e) => report.skip(member, failed(&e)),
        }
    }
    // A dir left writable is what its tool set out to prevent: the run stops and says where.
    // Every dir is put back before the first failure is told.
    let mut first = None;
    for (dir, mode) in lifted {
        if let Err(e) = sys::set_mode(&dir, mode) {
            first.get_or_insert_with(|| {
                io::Error::new(
                    e.kind(),
                    format!("{}: putting its mode {mode:o} back: {e}", dir.display()),
                )
            });
        }
    }
    first.map_or(Ok(()), Err)
}

/// Lifts the owner write bit of every read-only dir that holds a member of `batch`, so a temp
/// copy can be made and renamed there. Returns each lifted dir with the mode to put back. A dir
/// that cannot be lifted is left as it is, and its members fail as they would have. On Windows a
/// read-only dir does not stop anyone from creating files in it: nothing to lift.
fn lift_dirs(batch: &[Inode]) -> Vec<(PathBuf, u32)> {
    const OWNER_WRITE: u32 = 0o200;
    let mut lifted: Vec<(PathBuf, u32)> = Vec::new();
    if !cfg!(unix) {
        return lifted;
    }
    for dir in batch
        .iter()
        .flat_map(|member| &member.paths)
        .filter_map(|path| path.parent())
    {
        if lifted.iter().any(|(done, _)| done == dir) {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(dir) else {
            continue;
        };
        let mode = sys::mode(&meta);
        if mode & OWNER_WRITE == 0 && sys::set_mode(dir, mode | OWNER_WRITE).is_ok() {
            lifted.push((dir.to_path_buf(), mode));
        }
    }
    lifted
}

/// A private copy of the group's content next to its first path. Where the filesystem clones,
/// the copy shares the source's blocks. Where it only compresses (NTFS), the copy is a real
/// one and renaming it into place drops the uncompressed bytes.
fn stage_copy(member: &Inode, locked: &[PathBuf]) -> io::Result<Result<PathBuf, Skip>> {
    if let Some(skip) = check_group(member, locked)? {
        return Ok(Err(skip));
    }
    let source = &member.paths[0];
    let copy = sibling_temp(source);
    let staged = if sys::caps(source).clone {
        clone_as(source, &copy, member)
    } else {
        copy_as(source, &copy, member)
    };
    if let Err(error) = staged {
        let _ = fs::remove_file(&copy);
        return Err(error);
    }
    Ok(Ok(copy))
}

/// Swaps a compressed copy in, or removes the copy. Returns the inode now at the paths.
fn finish_compress(
    member: &Inode,
    copy: &Path,
    locked: &[PathBuf],
) -> io::Result<Result<Inode, Skip>> {
    let outcome = try_finish_compress(member, copy, locked);
    if !matches!(outcome, Ok(Ok(_))) {
        let _ = fs::remove_file(copy);
    }
    outcome
}

fn try_finish_compress(
    member: &Inode,
    copy: &Path,
    locked: &[PathBuf],
) -> io::Result<Result<Inode, Skip>> {
    let compressed = Inode::read(copy)?;
    if compressed.flags & COMPRESSED == 0 || compressed.stamp.size != member.stamp.size {
        return Ok(Err(Skip::NotCompressed));
    }
    // The backend took its time: look at the group once more.
    if let Some(skip) = check_group(member, locked)? {
        return Ok(Err(skip));
    }
    swap_in(copy, member, || restore_meta(copy, member))?;
    Inode::read(&member.paths[0]).map(Ok)
}

/// Puts the inode that `prepare` leaves at `temp` at every path of `member`: `rename` over the
/// first path, a `hard_link` plus `rename` for each other one. A crash leaves, per path, the old
/// file or the new one, never a partial file. A group cut in half still holds identical bytes
/// and the next run joins it again.
fn swap_in(
    temp: &Path,
    member: &Inode,
    prepare: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    let Some((first, rest)) = member.paths.split_first() else {
        return Ok(());
    };
    rename_over(temp, first, prepare)?;
    for path in rest {
        let temp = sibling_temp(path);
        rename_over(&temp, path, || fs::hard_link(first, &temp))?;
    }
    Ok(())
}

/// Runs `prepare`, then renames `temp` over `path`; removes `temp` when either step fails.
fn rename_over(
    temp: &Path,
    path: &Path,
    prepare: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    let result = prepare().and_then(|()| fs::rename(temp, path));
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

/// A hardlink instead of a clone. There is no metadata to restore — the inode is the source's
/// and so are its mode and times — except that the shared inode keeps the *later* of the two
/// modification times. A file that suddenly reads older than what it was built from is a file
/// cargo rebuilds, and that would make the pass cost a build instead of saving space. The
/// source's own mtime moves forward with it, which is the safe direction.
fn link_as(source: &Path, temp: &Path, member: &Inode) -> io::Result<()> {
    fs::hard_link(source, temp)?;
    if member.stamp.mtime > fs::symlink_metadata(temp)?.modified()? {
        sys::set_mtime(temp, member.stamp.mtime)?;
    }
    Ok(())
}

fn clone_as(source: &Path, temp: &Path, member: &Inode) -> io::Result<()> {
    sys::clone_file(source, temp)?;
    restore_meta(temp, member)
}

fn copy_as(source: &Path, temp: &Path, member: &Inode) -> io::Result<()> {
    fs::copy(source, temp)?;
    restore_meta(temp, member)
}

fn restore_meta(temp: &Path, member: &Inode) -> io::Result<()> {
    // Times first: a read-only mode would not stop `futimens`, but an unreadable one stops `open`.
    // On Windows `File::open` does not grant `FILE_WRITE_ATTRIBUTES`, so the time goes through
    // the backend, which opens the handle the platform needs.
    sys::set_mtime(temp, member.stamp.mtime)?;
    sys::set_mode(temp, member.mode)
}

fn sibling_temp(path: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let name = format!(
        "{TMP_PREFIX}{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_in_use_is_busy_and_other_errors_are_failures() {
        for kind in [
            io::ErrorKind::ExecutableFileBusy,
            io::ErrorKind::ResourceBusy,
        ] {
            assert_eq!(failed(&io::Error::from(kind)), Skip::Busy);
        }
        assert_eq!(
            failed(&io::Error::from(io::ErrorKind::PermissionDenied)),
            Skip::Failed(io::ErrorKind::PermissionDenied)
        );
        let sharing = io::Error::from_raw_os_error(SHARING_VIOLATION);
        let denied = io::Error::from_raw_os_error(ACCESS_DENIED);
        if cfg!(windows) {
            assert_eq!(failed(&sharing), Skip::Busy);
            assert_eq!(failed(&denied), Skip::Busy);
        } else {
            assert_ne!(failed(&sharing), Skip::Busy, "EPIPE on unix");
            assert_ne!(failed(&denied), Skip::Busy);
        }
    }
}
