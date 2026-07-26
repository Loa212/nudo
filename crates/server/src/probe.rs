//! Target reachability checks.
//!
//! `CheckTarget` answers the question an operator actually has when a deploy
//! fails: which part is broken? So it reports each prerequisite separately —
//! SSH, sudo, systemd, and a writable release directory — rather than one
//! pass/fail.

use nudo_proto::check_target_response::Check;

use crate::ssh::{SshSession, SshTarget, quote};

/// The names of the checks, in the order they are reported.
pub const CHECK_NAMES: [&str; 4] = ["ssh", "sudo", "systemd", "release_dir"];

/// Runs every prerequisite check against a target.
///
/// A failed SSH connection short-circuits the rest: without a connection the
/// other three cannot be evaluated, and reporting them as "failed" would imply
/// we looked.
pub async fn check_target(ssh_target: &SshTarget, release_root: &str) -> (bool, Vec<Check>) {
    let session = match SshSession::connect(ssh_target).await {
        Ok(session) => session,
        Err(error) => {
            let mut checks = vec![Check {
                name: "ssh".to_string(),
                ok: false,
                detail: format!("{error:#}"),
            }];
            for name in &CHECK_NAMES[1..] {
                checks.push(Check {
                    name: (*name).to_string(),
                    ok: false,
                    detail: "not checked: no SSH connection".to_string(),
                });
            }
            return (false, checks);
        }
    };

    let mut checks = Vec::with_capacity(CHECK_NAMES.len());

    // ---- ssh ----
    let whoami = session.exec("id -un").await;
    match &whoami {
        Ok(result) if result.ok() => checks.push(Check {
            name: "ssh".to_string(),
            ok: true,
            detail: format!("connected as {}", result.trimmed()),
        }),
        Ok(result) => checks.push(Check {
            name: "ssh".to_string(),
            ok: false,
            detail: format!("connected but `id -un` failed: {}", result.stderr.trim()),
        }),
        Err(error) => checks.push(Check {
            name: "ssh".to_string(),
            ok: false,
            detail: format!("{error:#}"),
        }),
    }

    let user = whoami
        .as_ref()
        .map(|r| r.trimmed().to_string())
        .unwrap_or_default();
    let is_root = user == "root";

    // ---- sudo ----
    // root needs no sudo, and requiring it would fail a perfectly good target.
    if is_root {
        checks.push(Check {
            name: "sudo".to_string(),
            ok: true,
            detail: "running as root; sudo not required".to_string(),
        });
    } else {
        // `-n` so a target configured to prompt fails immediately rather than
        // hanging until the SSH timeout.
        let sudo = session.exec("sudo -n true 2>&1").await;
        match sudo {
            Ok(result) if result.ok() => checks.push(Check {
                name: "sudo".to_string(),
                ok: true,
                detail: "passwordless sudo available".to_string(),
            }),
            Ok(result) => checks.push(Check {
                name: "sudo".to_string(),
                ok: false,
                detail: format!(
                    "passwordless sudo is required to manage units as {user}: {}",
                    first_line(&result.stdout, &result.stderr)
                ),
            }),
            Err(error) => checks.push(Check {
                name: "sudo".to_string(),
                ok: false,
                detail: format!("{error:#}"),
            }),
        }
    }

    // ---- systemd ----
    let systemd = session.exec("systemctl --version 2>&1 | head -n 1").await;
    match systemd {
        Ok(result) if result.ok() && !result.trimmed().is_empty() => checks.push(Check {
            name: "systemd".to_string(),
            ok: true,
            detail: result.trimmed().to_string(),
        }),
        Ok(result) => checks.push(Check {
            name: "systemd".to_string(),
            ok: false,
            detail: format!(
                "systemctl is not usable on this host: {}",
                first_line(&result.stdout, &result.stderr)
            ),
        }),
        Err(error) => checks.push(Check {
            name: "systemd".to_string(),
            ok: false,
            detail: format!("{error:#}"),
        }),
    }

    // ---- release dir ----
    // Checks that the directory can be created and written, not merely that it
    // exists — the first deploy has to create it.
    let root = if release_root.trim().is_empty() {
        "/opt".to_string()
    } else {
        release_root.trim().trim_end_matches('/').to_string()
    };
    let probe_file = format!("{root}/.nudo-write-probe");
    let command = format!(
        "mkdir -p {root} && touch {probe} && rm -f {probe} && echo ok",
        root = quote(&root),
        probe = quote(&probe_file)
    );

    match session.exec(&command).await {
        Ok(result) if result.ok() && result.trimmed() == "ok" => checks.push(Check {
            name: "release_dir".to_string(),
            ok: true,
            detail: format!("{root} is writable"),
        }),
        Ok(result) => checks.push(Check {
            name: "release_dir".to_string(),
            ok: false,
            detail: format!(
                "{root} is not writable: {}",
                first_line(&result.stdout, &result.stderr)
            ),
        }),
        Err(error) => checks.push(Check {
            name: "release_dir".to_string(),
            ok: false,
            detail: format!("{error:#}"),
        }),
    }

    let _ = session.close().await;
    let ok = checks.iter().all(|check| check.ok);
    (ok, checks)
}

/// The first non-empty line of stderr, or of stdout when stderr is empty.
///
/// Commands here are run with `2>&1` in places, so the useful message can be on
/// either stream; a whole multi-line dump would not fit a UI row.
fn first_line(stdout: &str, stderr: &str) -> String {
    for source in [stderr, stdout] {
        if let Some(line) = source.lines().map(str::trim).find(|l| !l.is_empty()) {
            return line.to_string();
        }
    }
    "no output".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_useful_message_is_taken_from_whichever_stream_has_one() {
        assert_eq!(first_line("out", "err"), "err", "stderr wins when present");
        assert_eq!(first_line("out", ""), "out");
        assert_eq!(first_line("", "  \n  err  \n"), "err");
        // Multi-line output is reduced to one line for display.
        assert_eq!(first_line("first\nsecond", ""), "first");
        assert_eq!(first_line("", ""), "no output");
        assert_eq!(first_line("\n\n", "\n"), "no output");
    }

    #[tokio::test]
    async fn an_unreachable_target_reports_ssh_as_the_cause_and_the_rest_as_unchecked() {
        // Reporting the other three as plain failures would imply we looked at
        // them, sending an operator to debug sudo when the host is simply down.
        let unreachable = SshTarget {
            // Reserved for documentation, so it never resolves to a real host.
            host: "192.0.2.1".to_string(),
            port: 22,
            user: "root".to_string(),
            private_key: "-----BEGIN OPENSSH PRIVATE KEY-----\ninvalid\n".to_string(),
            passphrase: None,
        };

        let (ok, checks) = check_target(&unreachable, "/opt/bot").await;
        assert!(!ok);
        assert_eq!(checks.len(), CHECK_NAMES.len());

        let names: Vec<&str> = checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, CHECK_NAMES.to_vec());

        assert!(!checks[0].ok);
        assert!(!checks[0].detail.is_empty());
        for check in &checks[1..] {
            assert!(!check.ok);
            assert!(
                check.detail.contains("no SSH connection"),
                "expected an unchecked marker, got: {}",
                check.detail
            );
        }
    }

    #[test]
    fn the_reported_checks_are_exactly_the_ones_the_proto_documents() {
        assert_eq!(CHECK_NAMES, ["ssh", "sudo", "systemd", "release_dir"]);
    }
}
