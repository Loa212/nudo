//! `nudo-boot-guard <self_dir>` — the crash-loop backstop for self-upgrades.
//!
//! Runs as `ExecStartPre=` on every start of the nudo unit, from a stable
//! path deliberately *outside* the `current` symlink (a broken release's own
//! guard could be broken too; the stable copy is refreshed only after a
//! release confirms a healthy boot).
//!
//! The contract, in order of importance:
//!
//! 1. Never block a start it does not have to. Any problem with the guard's
//!    own bookkeeping — an unreadable journal, a corrupt counter — is logged
//!    to stderr and ignored, because failing `ExecStartPre` would turn a
//!    bookkeeping bug into an outage on a service that might be fine.
//! 2. Count starts only while an upgrade is swapped-but-unconfirmed. Every
//!    start in that state increments a counter; the running binary confirms
//!    the upgrade on a successful boot, which resets it.
//! 3. At the limit, revert. The counter reaching MAX_BOOT_ATTEMPTS means the
//!    swapped release was started repeatedly and never confirmed — including
//!    the case where it could not even exec. The guard points `current` back
//!    at the previous release and records the rollback, then exits 0 so
//!    systemd starts the restored binary.

use std::process::ExitCode;

use nudo_bootguard::{Journal, JournalState, MAX_BOOT_ATTEMPTS};

fn main() -> ExitCode {
    let Some(self_dir) = std::env::args_os().nth(1).map(std::path::PathBuf::from) else {
        eprintln!("usage: nudo-boot-guard <self_dir>");
        // The one case that does fail the start: being run without the
        // directory means the unit file is wrong, and starting anyway would
        // mean running unguarded forever without anyone noticing.
        return ExitCode::FAILURE;
    };

    let journal = match Journal::load(&self_dir) {
        Ok(Some(journal)) => journal,
        Ok(None) => {
            // No upgrade in flight (or a journal this guard cannot read,
            // which must mean a newer format). Make sure no stale counter
            // lingers and let the start proceed.
            reset_attempts(&self_dir);
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("nudo-boot-guard: journal unreadable, standing down: {error}");
            return ExitCode::SUCCESS;
        }
    };

    if journal.state != JournalState::Swapped {
        // Staged, confirmed, failed, rolled back: nothing to guard.
        reset_attempts(&self_dir);
        return ExitCode::SUCCESS;
    }

    let attempts = nudo_bootguard::read_attempts(&self_dir) + 1;
    if attempts < MAX_BOOT_ATTEMPTS {
        if let Err(error) = nudo_bootguard::write_attempts(&self_dir, attempts) {
            eprintln!("nudo-boot-guard: could not count this start: {error}");
        }
        eprintln!(
            "nudo-boot-guard: start {attempts}/{MAX_BOOT_ATTEMPTS} for unconfirmed {}",
            journal.to_version
        );
        return ExitCode::SUCCESS;
    }

    // The swapped release has had its chances. Revert the symlink first —
    // that is the operation that matters — then record what happened.
    eprintln!(
        "nudo-boot-guard: {} never confirmed after {attempts} starts; reverting to {}",
        journal.to_version, journal.from_version
    );
    if let Err(error) = nudo_bootguard::swap_current(&self_dir, &journal.previous) {
        // Reverting failed: leave everything as it is. systemd will start the
        // swapped release again and this guard will retry the revert on the
        // next pass — exiting non-zero here would only remove the retries.
        eprintln!("nudo-boot-guard: reverting the symlink failed: {error}");
        return ExitCode::SUCCESS;
    }

    let rolled_back = Journal {
        state: JournalState::RolledBack,
        updated_at: nudo_bootguard::epoch_seconds(),
        error: format!(
            "{} crash-looped ({attempts} starts without confirming); reverted to {}",
            journal.to_version, journal.from_version
        ),
        ..journal
    };
    if let Err(error) = rolled_back.store(&self_dir) {
        eprintln!("nudo-boot-guard: recording the rollback failed: {error}");
    }
    reset_attempts(&self_dir);
    ExitCode::SUCCESS
}

fn reset_attempts(self_dir: &std::path::Path) {
    if let Err(error) = nudo_bootguard::clear_attempts(self_dir) {
        eprintln!("nudo-boot-guard: could not clear the attempts counter: {error}");
    }
}
