//! The self-upgrade journal and the atomic `current` swap.
//!
//! Shared by the control plane (which stages an upgrade, swaps the symlink and
//! execs itself) and `nudo-boot-guard` (which runs as `ExecStartPre=` and
//! reverts the swap when the new binary cannot stay up). Sharing matters: the
//! swap the guard performs to undo an upgrade must be the same tested code as
//! the swap that made it.
//!
//! Everything here is std-only and file-based on purpose. The guard runs
//! precisely when things are broken — possibly including the database and the
//! new binary — so its inputs are a line-based text file and a counter file,
//! not JSON and not SQLite.
//!
//! Layout under the self directory (`/var/lib/nudo/self` in the packaged
//! unit):
//!
//! ```text
//! releases/<version>/   one directory per staged release, binaries inside
//! current               symlink to releases/<version>; ExecStart points here
//! journal               the state of the most recent upgrade, key=value lines
//! boot-attempts         starts since the swap, reset once a version confirms
//! nudo-boot-guard       stable guard copy, refreshed only after confirmation
//! ```

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The journal file name under the self directory.
pub const JOURNAL_FILE: &str = "journal";

/// The boot-attempts counter file name under the self directory.
pub const ATTEMPTS_FILE: &str = "boot-attempts";

/// The symlink the unit's `ExecStart` resolves through.
pub const CURRENT_LINK: &str = "current";

/// How many starts a swapped-but-unconfirmed release gets before the guard
/// reverts it. Three covers a slow first boot and one unlucky crash without
/// letting a broken release restart-loop for long.
pub const MAX_BOOT_ATTEMPTS: u32 = 3;

/// Where an upgrade is in its lifecycle.
///
/// `Staged` and `Swapped` are transient; everything else is a resting state
/// the dashboard can show. Only `Swapped` makes the guard count boots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalState {
    /// The new release is on disk and verified; `current` still points at the
    /// old one.
    Staged,
    /// `current` points at the new release, which has not yet confirmed a
    /// healthy boot.
    Swapped,
    /// The new release booted and confirmed itself.
    Confirmed,
    /// exec() of the new binary failed and returned; the old process swapped
    /// `current` back and kept running.
    ExecFailed,
    /// The guard reverted `current` after too many unconfirmed boots.
    RolledBack,
    /// The upgrade failed before the swap, or a swapped release never came up
    /// as the version it promised.
    Failed,
}

impl JournalState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Staged => "staged",
            Self::Swapped => "swapped",
            Self::Confirmed => "confirmed",
            Self::ExecFailed => "exec-failed",
            Self::RolledBack => "rolled-back",
            Self::Failed => "failed",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "staged" => Self::Staged,
            "swapped" => Self::Swapped,
            "confirmed" => Self::Confirmed,
            "exec-failed" => Self::ExecFailed,
            "rolled-back" => Self::RolledBack,
            "failed" => Self::Failed,
            _ => return None,
        })
    }
}

/// The record of the most recent upgrade attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Journal {
    pub state: JournalState,
    /// The version that started the upgrade.
    pub from_version: String,
    /// The version being upgraded to.
    pub to_version: String,
    /// The release directory to revert to, relative to the self directory
    /// (`releases/<version>`). Relative so the layout can be moved or
    /// inspected from outside without the paths lying.
    pub previous: String,
    /// The release directory being activated, relative to the self directory.
    pub target: String,
    /// Unix epoch seconds of the last state change. Epoch rather than a
    /// formatted date so the guard does not need a calendar to write one.
    pub updated_at: u64,
    /// Human-readable detail for the failure states, empty otherwise.
    pub error: String,
}

impl Journal {
    /// Reads the journal, if one exists and is intelligible.
    ///
    /// A missing file and an unintelligible one both come back as `None`: the
    /// guard must treat "cannot read the journal" as "do nothing", because
    /// failing the unit's start over a corrupt bookkeeping file would turn a
    /// cosmetic problem into an outage.
    pub fn load(self_dir: &Path) -> io::Result<Option<Journal>> {
        let raw = match fs::read_to_string(self_dir.join(JOURNAL_FILE)) {
            Ok(raw) => raw,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };

        let mut state = None;
        let mut journal = Journal {
            state: JournalState::Failed,
            from_version: String::new(),
            to_version: String::new(),
            previous: String::new(),
            target: String::new(),
            updated_at: 0,
            error: String::new(),
        };

        for line in raw.lines() {
            // Split on the first `=` only, so values may contain the character.
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key {
                "state" => state = JournalState::parse(value),
                "from_version" => journal.from_version = value.to_string(),
                "to_version" => journal.to_version = value.to_string(),
                "previous" => journal.previous = value.to_string(),
                "target" => journal.target = value.to_string(),
                "updated_at" => journal.updated_at = value.parse().unwrap_or(0),
                "error" => journal.error = value.to_string(),
                // Unknown keys are tolerated: an older guard reading a newer
                // journal must not choke on a field it never learned.
                _ => {}
            }
        }

        Ok(state.map(|state| Journal { state, ..journal }))
    }

    /// Writes the journal atomically: temp file, fsync, rename, fsync the
    /// directory. A torn journal read as `None` would silently disarm the
    /// guard, so the write must never be observable half-done.
    pub fn store(&self, self_dir: &Path) -> io::Result<()> {
        // The error line must stay one line, or the parser would drop the rest.
        let error = self.error.replace(['\n', '\r'], "; ");
        let rendered = format!(
            "state={}\nfrom_version={}\nto_version={}\nprevious={}\ntarget={}\nupdated_at={}\nerror={}\n",
            self.state.as_str(),
            self.from_version,
            self.to_version,
            self.previous,
            self.target,
            self.updated_at,
            error,
        );
        write_atomically(&self_dir.join(JOURNAL_FILE), rendered.as_bytes())?;
        fsync_dir(self_dir)
    }
}

/// The current time as unix epoch seconds, for `Journal::updated_at`.
pub fn epoch_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Reads the boot-attempts counter. Anything unreadable counts as zero — a
/// corrupt counter must err towards giving the release more chances, never
/// towards reverting a healthy one early.
pub fn read_attempts(self_dir: &Path) -> u32 {
    fs::read_to_string(self_dir.join(ATTEMPTS_FILE))
        .ok()
        .and_then(|raw| raw.trim().parse().ok())
        .unwrap_or(0)
}

/// Writes the boot-attempts counter, atomically for the same reason as the
/// journal.
pub fn write_attempts(self_dir: &Path, attempts: u32) -> io::Result<()> {
    write_atomically(
        &self_dir.join(ATTEMPTS_FILE),
        attempts.to_string().as_bytes(),
    )?;
    fsync_dir(self_dir)
}

/// Removes the boot-attempts counter, used once a release confirms.
pub fn clear_attempts(self_dir: &Path) -> io::Result<()> {
    match fs::remove_file(self_dir.join(ATTEMPTS_FILE)) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Points `current` at a release directory, atomically.
///
/// The local port of the deploy engine's remote idiom (`ln -sfn` + `mv -T` in
/// `deploy/execution.rs`): create the symlink under a temporary name, then
/// rename it over `current`. `rename` replaces an existing symlink atomically
/// on Unix, so a process resolving `current` sees either the old target or the
/// new one, never nothing.
///
/// `release_rel` is relative to the self directory (`releases/<version>`), and
/// the link stores it relative, so the whole layout can be relocated.
pub fn swap_current(self_dir: &Path, release_rel: &str) -> io::Result<()> {
    let temp = self_dir.join("current.tmp");
    // A leftover temp link from an interrupted swap would fail symlink().
    match fs::remove_file(&temp) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    std::os::unix::fs::symlink(release_rel, &temp)?;
    fs::rename(&temp, self_dir.join(CURRENT_LINK))?;
    fsync_dir(self_dir)
}

/// Where `current` points, relative to the self directory, if it exists.
pub fn current_target(self_dir: &Path) -> Option<PathBuf> {
    fs::read_link(self_dir.join(CURRENT_LINK)).ok()
}

fn write_atomically(path: &Path, contents: &[u8]) -> io::Result<()> {
    let temp = path.with_extension("tmp");
    {
        use std::io::Write as _;
        let mut file = fs::File::create(&temp)?;
        file.write_all(contents)?;
        file.sync_all()?;
    }
    fs::rename(&temp, path)
}

/// Flushes a directory's entries to disk, so a rename survives power loss.
fn fsync_dir(dir: &Path) -> io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn journal() -> Journal {
        Journal {
            state: JournalState::Swapped,
            from_version: "0.3.0".to_string(),
            to_version: "0.4.0".to_string(),
            previous: "releases/0.3.0".to_string(),
            target: "releases/0.4.0".to_string(),
            updated_at: 1_800_000_000,
            error: String::new(),
        }
    }

    fn tempdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nudo-bootguard-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id(),
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create tempdir");
        dir
    }

    #[test]
    fn a_journal_round_trips() {
        let dir = tempdir();
        journal().store(&dir).expect("store");
        assert_eq!(Journal::load(&dir).expect("load"), Some(journal()));
    }

    #[test]
    fn a_missing_journal_is_none_not_an_error() {
        assert_eq!(Journal::load(&tempdir()).expect("load"), None);
    }

    #[test]
    fn an_unknown_state_reads_as_none() {
        // A newer journal format must never make an older guard act on a
        // state it does not understand.
        let dir = tempdir();
        fs::write(dir.join(JOURNAL_FILE), "state=defenestrated\n").expect("write");
        assert_eq!(Journal::load(&dir).expect("load"), None);
    }

    #[test]
    fn unknown_keys_are_tolerated() {
        let dir = tempdir();
        fs::write(
            dir.join(JOURNAL_FILE),
            "state=confirmed\nto_version=1.0.0\nfuture_field=whatever\n",
        )
        .expect("write");
        let loaded = Journal::load(&dir).expect("load").expect("some");
        assert_eq!(loaded.state, JournalState::Confirmed);
        assert_eq!(loaded.to_version, "1.0.0");
    }

    #[test]
    fn a_multiline_error_stays_on_one_line() {
        // A newline inside the error would truncate the parse at the next
        // read, dropping every field after it.
        let dir = tempdir();
        let mut record = journal();
        record.error = "first line\nsecond line".to_string();
        record.store(&dir).expect("store");
        let loaded = Journal::load(&dir).expect("load").expect("some");
        assert_eq!(loaded.error, "first line; second line");
    }

    #[test]
    fn the_attempts_counter_counts_and_clears() {
        let dir = tempdir();
        assert_eq!(read_attempts(&dir), 0, "absent counts as zero");
        write_attempts(&dir, 2).expect("write");
        assert_eq!(read_attempts(&dir), 2);
        clear_attempts(&dir).expect("clear");
        assert_eq!(read_attempts(&dir), 0);
        clear_attempts(&dir).expect("clearing twice is fine");
    }

    #[test]
    fn a_corrupt_attempts_file_counts_as_zero() {
        // Erring towards more chances, never towards reverting early.
        let dir = tempdir();
        fs::write(dir.join(ATTEMPTS_FILE), "not a number").expect("write");
        assert_eq!(read_attempts(&dir), 0);
    }

    #[test]
    fn swap_current_repoints_an_existing_link() {
        let dir = tempdir();
        fs::create_dir_all(dir.join("releases/0.3.0")).expect("mkdir");
        fs::create_dir_all(dir.join("releases/0.4.0")).expect("mkdir");

        swap_current(&dir, "releases/0.3.0").expect("first swap");
        assert_eq!(
            current_target(&dir),
            Some(PathBuf::from("releases/0.3.0")),
            "the link is created when absent"
        );

        swap_current(&dir, "releases/0.4.0").expect("second swap");
        assert_eq!(
            current_target(&dir),
            Some(PathBuf::from("releases/0.4.0")),
            "the link is replaced when present"
        );
    }

    #[test]
    fn swap_current_survives_a_leftover_temp_link() {
        // An interrupted earlier swap leaves current.tmp behind; the next swap
        // must clean it up rather than fail forever.
        let dir = tempdir();
        std::os::unix::fs::symlink("releases/stale", dir.join("current.tmp")).expect("leftover");
        swap_current(&dir, "releases/0.4.0").expect("swap despite leftover");
        assert_eq!(current_target(&dir), Some(PathBuf::from("releases/0.4.0")));
    }
}
