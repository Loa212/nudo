//! Behaviour tests for the `nudo-boot-guard` binary — the real one, spawned
//! the way systemd spawns it, because the whole point of the guard is what it
//! does as a process.

use std::path::{Path, PathBuf};
use std::process::Command;

use nudo_bootguard::{Journal, JournalState, MAX_BOOT_ATTEMPTS};

fn guard() -> Command {
    Command::new(env!("CARGO_BIN_EXE_nudo-boot-guard"))
}

fn tempdir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nudo-guard-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

fn swapped_journal() -> Journal {
    Journal {
        state: JournalState::Swapped,
        from_version: "0.3.0".to_string(),
        to_version: "0.4.0".to_string(),
        previous: "releases/0.3.0".to_string(),
        target: "releases/0.4.0".to_string(),
        updated_at: 1,
        error: String::new(),
    }
}

fn layout(dir: &Path) {
    std::fs::create_dir_all(dir.join("releases/0.3.0")).expect("mkdir");
    std::fs::create_dir_all(dir.join("releases/0.4.0")).expect("mkdir");
    nudo_bootguard::swap_current(dir, "releases/0.4.0").expect("link");
}

#[test]
fn without_a_journal_the_guard_stands_down_and_clears_the_counter() {
    let dir = tempdir("no-journal");
    nudo_bootguard::write_attempts(&dir, 2).expect("stale counter");

    let status = guard().arg(&dir).status().expect("run");

    assert!(status.success());
    assert_eq!(
        nudo_bootguard::read_attempts(&dir),
        0,
        "a stale counter from a finished upgrade must not haunt the next one"
    );
}

#[test]
fn a_swapped_release_gets_its_starts_counted_but_not_reverted_early() {
    let dir = tempdir("counting");
    layout(&dir);
    swapped_journal().store(&dir).expect("journal");

    for expected in 1..MAX_BOOT_ATTEMPTS {
        let status = guard().arg(&dir).status().expect("run");
        assert!(status.success());
        assert_eq!(nudo_bootguard::read_attempts(&dir), expected);
        assert_eq!(
            nudo_bootguard::current_target(&dir),
            Some(PathBuf::from("releases/0.4.0")),
            "the release keeps its chances until the limit"
        );
    }
}

#[test]
fn at_the_limit_the_guard_reverts_and_records_the_rollback() {
    let dir = tempdir("revert");
    layout(&dir);
    swapped_journal().store(&dir).expect("journal");
    nudo_bootguard::write_attempts(&dir, MAX_BOOT_ATTEMPTS - 1).expect("counter");

    let status = guard().arg(&dir).status().expect("run");

    assert!(status.success(), "the start proceeds — with the old binary");
    assert_eq!(
        nudo_bootguard::current_target(&dir),
        Some(PathBuf::from("releases/0.3.0")),
        "current points back at the previous release"
    );
    let journal = Journal::load(&dir).expect("load").expect("some");
    assert_eq!(journal.state, JournalState::RolledBack);
    assert!(journal.error.contains("crash-looped"), "{}", journal.error);
    assert_eq!(nudo_bootguard::read_attempts(&dir), 0);
}

#[test]
fn resting_states_are_not_counted_or_touched() {
    for state in [
        JournalState::Staged,
        JournalState::Confirmed,
        JournalState::ExecFailed,
        JournalState::RolledBack,
        JournalState::Failed,
    ] {
        let dir = tempdir(&format!("resting-{}", state.as_str()));
        layout(&dir);
        Journal {
            state,
            ..swapped_journal()
        }
        .store(&dir)
        .expect("journal");

        let status = guard().arg(&dir).status().expect("run");

        assert!(status.success());
        assert_eq!(nudo_bootguard::read_attempts(&dir), 0);
        assert_eq!(
            nudo_bootguard::current_target(&dir),
            Some(PathBuf::from("releases/0.4.0")),
            "{state:?} must not move the symlink"
        );
    }
}

#[test]
fn a_corrupt_journal_stands_the_guard_down_rather_than_blocking_the_start() {
    let dir = tempdir("corrupt");
    layout(&dir);
    std::fs::write(dir.join("journal"), "state=unintelligible\n").expect("write");

    let status = guard().arg(&dir).status().expect("run");

    assert!(
        status.success(),
        "a bookkeeping bug must never be an outage"
    );
    assert_eq!(
        nudo_bootguard::current_target(&dir),
        Some(PathBuf::from("releases/0.4.0")),
        "and must not move the symlink either"
    );
}

#[test]
fn a_corrupt_attempts_file_restarts_the_count_instead_of_reverting() {
    let dir = tempdir("corrupt-counter");
    layout(&dir);
    swapped_journal().store(&dir).expect("journal");
    std::fs::write(dir.join("boot-attempts"), "garbage").expect("write");

    let status = guard().arg(&dir).status().expect("run");

    assert!(status.success());
    assert_eq!(
        nudo_bootguard::read_attempts(&dir),
        1,
        "garbage reads as zero, so this start counts as the first"
    );
    assert_eq!(
        nudo_bootguard::current_target(&dir),
        Some(PathBuf::from("releases/0.4.0"))
    );
}

#[test]
fn running_without_an_argument_fails_the_start() {
    // The one deliberate failure: a wrong unit file means running unguarded
    // forever, and nobody would notice until an upgrade crash-looped.
    let status = guard().status().expect("run");
    assert!(!status.success());
}
