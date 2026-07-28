//! Target and build-host reachability checks.
//!
//! `CheckTarget` answers the question an operator actually has when a deploy
//! fails: which part is broken? So it reports each prerequisite separately —
//! the host key, SSH, sudo, systemd, and a writable release directory — rather
//! than one pass/fail.
//!
//! `check_build_host` answers the same question for the other role, against the
//! things a build actually needs: a verified identity, SSH, a writable
//! workspace, and git. It deliberately does not check for sudo or systemd —
//! nothing is installed or supervised on a build host — which is the clearest
//! statement that the two roles are not the same machine.

use nudo_proto::check_build_host_response::Check as BuildHostCheck;
use nudo_proto::check_target_response::Check;

use crate::ssh::{HostKeyChanged, HostKeyOutcome, SshSession, quote};

/// The names of the checks, in the order they are reported.
pub const CHECK_NAMES: [&str; 5] = ["host_key", "ssh", "sudo", "systemd", "release_dir"];

/// The names of the build-host checks, in the order they are reported.
pub const BUILD_HOST_CHECK_NAMES: [&str; 4] = ["host_key", "ssh", "workspace", "git"];

/// Runs every prerequisite check against an already-open session.
///
/// The connection is made by the caller rather than here so that whichever
/// layer owns the store records the host-key outcome; this function is handed
/// the result either way. A failed connection short-circuits the rest: without
/// one the others cannot be evaluated, and reporting them as "failed" would
/// imply we looked.
pub async fn check_target(
    connection: anyhow::Result<SshSession>,
    release_root: &str,
) -> (bool, Vec<Check>) {
    let session = match connection {
        Ok(session) => session,
        Err(error) => return (false, unreachable_checks(&error)),
    };

    let mut checks = Vec::with_capacity(CHECK_NAMES.len());

    // ---- host key ----
    // First, because it is the check that decides whether the rest are even
    // being run against the machine we think they are.
    checks.push(match session.host_key() {
        HostKeyOutcome::Pinned { fingerprint, .. } => Check {
            name: "host_key".to_string(),
            ok: true,
            detail: format!("pinned on first use: {fingerprint}"),
        },
        HostKeyOutcome::Matched { fingerprint } => Check {
            name: "host_key".to_string(),
            ok: true,
            detail: format!("matches the pinned key: {fingerprint}"),
        },
        // Not reachable through a live session — a change fails the connect —
        // but reported honestly rather than asserted away.
        HostKeyOutcome::Changed { fingerprint, .. } => Check {
            name: "host_key".to_string(),
            ok: false,
            detail: format!("the host key has changed: {fingerprint}"),
        },
    });

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

/// The checks reported when no session could be opened.
///
/// A refused host key is called out as the host-key check failing rather than
/// as SSH being down: the difference is the difference between "the box is
/// off" and "something else is answering for that address", and an operator
/// sent to debug the wrong one of those wastes the time that matters most.
fn unreachable_checks(error: &anyhow::Error) -> Vec<Check> {
    let host_key_changed = error.is::<HostKeyChanged>();
    // The check that actually failed carries the error; the rest say they were
    // not reached, and why.
    let blamed = if host_key_changed { "host_key" } else { "ssh" };
    let unchecked = if host_key_changed {
        "not checked: the host key was refused"
    } else {
        "not checked: no SSH connection"
    };
    let detail = format!("{error:#}");

    CHECK_NAMES
        .iter()
        .map(|name| Check {
            name: (*name).to_string(),
            ok: false,
            detail: if *name == blamed {
                detail.clone()
            } else {
                unchecked.to_string()
            },
        })
        .collect()
}

/// Runs every build-host prerequisite against an already-open session.
///
/// Returns the checks and any non-fatal warnings. A warning must never make the
/// result fail: "this host is latency-critical" is something the operator chose
/// and is entitled to keep, and turning it into a failed check would make an
/// intentional configuration look broken.
///
/// `latency_critical` is passed in rather than probed, because it is a property
/// of the registration rather than of the machine.
pub async fn check_build_host(
    connection: anyhow::Result<SshSession>,
    workspace_root: &str,
    latency_critical: bool,
) -> (bool, Vec<BuildHostCheck>, Vec<String>) {
    let mut warnings = Vec::new();
    if latency_critical {
        warnings.push(
            "This build host is marked latency-critical. A build will compete with whatever \
             else runs here for CPU, cache and memory bandwidth; expect jitter on anything \
             sensitive while a build is in flight."
                .to_string(),
        );
    }

    let session = match connection {
        Ok(session) => session,
        Err(error) => return (false, unreachable_build_host_checks(&error), warnings),
    };

    let mut checks = Vec::with_capacity(BUILD_HOST_CHECK_NAMES.len());

    // ---- host key ----
    // First, for the same reason as a target: it decides whether the rest are
    // being run against the machine we think they are. It matters at least as
    // much here, since this host is handed repository credentials.
    checks.push(match session.host_key() {
        HostKeyOutcome::Pinned { fingerprint, .. } => BuildHostCheck {
            name: "host_key".to_string(),
            ok: true,
            detail: format!("pinned on first use: {fingerprint}"),
        },
        HostKeyOutcome::Matched { fingerprint } => BuildHostCheck {
            name: "host_key".to_string(),
            ok: true,
            detail: format!("matches the pinned key: {fingerprint}"),
        },
        HostKeyOutcome::Changed { fingerprint, .. } => BuildHostCheck {
            name: "host_key".to_string(),
            ok: false,
            detail: format!("the host key has changed: {fingerprint}"),
        },
    });

    // ---- ssh ----
    let whoami = session.exec("id -un").await;
    match &whoami {
        Ok(result) if result.ok() => checks.push(BuildHostCheck {
            name: "ssh".to_string(),
            ok: true,
            detail: format!("connected as {}", result.trimmed()),
        }),
        Ok(result) => checks.push(BuildHostCheck {
            name: "ssh".to_string(),
            ok: false,
            detail: format!("connected but `id -un` failed: {}", result.stderr.trim()),
        }),
        Err(error) => checks.push(BuildHostCheck {
            name: "ssh".to_string(),
            ok: false,
            detail: format!("{error:#}"),
        }),
    }

    // ---- workspace ----
    // Creatable and writable, not merely present: the first build has to create
    // it, and a root that exists but is read-only fails every build with a
    // confusing clone error instead of this one.
    let root = if workspace_root.trim().is_empty() {
        crate::store::DEFAULT_WORKSPACE_ROOT.to_string()
    } else {
        workspace_root.trim().trim_end_matches('/').to_string()
    };
    let probe_file = format!("{root}/.nudo-write-probe");
    let command = format!(
        "mkdir -p {root} && touch {probe} && rm -f {probe} && echo ok",
        root = quote(&root),
        probe = quote(&probe_file)
    );

    match session.exec(&command).await {
        Ok(result) if result.ok() && result.trimmed() == "ok" => checks.push(BuildHostCheck {
            name: "workspace".to_string(),
            ok: true,
            detail: format!("{root} is writable"),
        }),
        Ok(result) => checks.push(BuildHostCheck {
            name: "workspace".to_string(),
            ok: false,
            detail: format!(
                "{root} is not writable: {}",
                first_line(&result.stdout, &result.stderr)
            ),
        }),
        Err(error) => checks.push(BuildHostCheck {
            name: "workspace".to_string(),
            ok: false,
            detail: format!("{error:#}"),
        }),
    }

    // ---- git ----
    // The one tool nudo itself runs on a build host. Everything else a build
    // needs is named by the build command, which nudo does not parse and will
    // not guess at; a missing compiler surfaces as a build failure with the
    // command's own error, which is more useful than anything probing could
    // invent.
    let git = session.exec("git --version 2>&1 | head -n 1").await;
    match git {
        Ok(result) if result.ok() && !result.trimmed().is_empty() => checks.push(BuildHostCheck {
            name: "git".to_string(),
            ok: true,
            detail: result.trimmed().to_string(),
        }),
        Ok(result) => checks.push(BuildHostCheck {
            name: "git".to_string(),
            ok: false,
            detail: format!(
                "git is not usable on this host, so nudo cannot clone here: {}",
                first_line(&result.stdout, &result.stderr)
            ),
        }),
        Err(error) => checks.push(BuildHostCheck {
            name: "git".to_string(),
            ok: false,
            detail: format!("{error:#}"),
        }),
    }

    let _ = session.close().await;
    let ok = checks.iter().all(|check| check.ok);
    (ok, checks, warnings)
}

/// The build-host checks reported when no session could be opened.
fn unreachable_build_host_checks(error: &anyhow::Error) -> Vec<BuildHostCheck> {
    let host_key_changed = error.is::<HostKeyChanged>();
    let blamed = if host_key_changed { "host_key" } else { "ssh" };
    let unchecked = if host_key_changed {
        "not checked: the host key was refused"
    } else {
        "not checked: no SSH connection"
    };
    let detail = format!("{error:#}");

    BUILD_HOST_CHECK_NAMES
        .iter()
        .map(|name| BuildHostCheck {
            name: (*name).to_string(),
            ok: false,
            detail: if *name == blamed {
                detail.clone()
            } else {
                unchecked.to_string()
            },
        })
        .collect()
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
        // Reporting the others as plain failures would imply we looked at them,
        // sending an operator to debug sudo when the host is simply down.
        let (ok, checks) =
            check_target(Err(anyhow::anyhow!("connection refused")), "/opt/bot").await;

        assert!(!ok);
        let names: Vec<&str> = checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, CHECK_NAMES.to_vec());

        let ssh = find(&checks, "ssh");
        assert!(!ssh.ok);
        assert!(ssh.detail.contains("connection refused"));

        for check in checks.iter().filter(|c| c.name != "ssh") {
            assert!(!check.ok);
            assert!(
                check.detail.contains("no SSH connection"),
                "expected an unchecked marker, got: {}",
                check.detail
            );
        }
    }

    #[tokio::test]
    async fn a_changed_host_key_is_blamed_on_the_host_key_rather_than_on_ssh() {
        // The difference between "the box is off" and "something else is
        // answering for that address" is the whole reason this check exists;
        // reporting the second as the first sends an operator to debug the
        // wrong thing entirely.
        let (ok, checks) = check_target(
            Err(HostKeyChanged {
                host: "10.0.0.5".to_string(),
                port: 22,
                expected_fingerprint: "SHA256:old".to_string(),
                key: "ssh-ed25519 AAAA".to_string(),
                fingerprint: "SHA256:new".to_string(),
            }
            .into()),
            "/opt/bot",
        )
        .await;

        assert!(!ok);
        let host_key = find(&checks, "host_key");
        assert!(!host_key.ok);
        assert!(
            host_key.detail.contains("SHA256:old"),
            "{}",
            host_key.detail
        );
        assert!(
            host_key.detail.contains("SHA256:new"),
            "{}",
            host_key.detail
        );

        // SSH is not blamed: the connection was refused deliberately.
        let ssh = find(&checks, "ssh");
        assert!(!ssh.ok);
        assert!(
            ssh.detail.contains("host key was refused"),
            "got: {}",
            ssh.detail
        );
    }

    #[test]
    fn the_reported_checks_are_exactly_the_ones_the_proto_documents() {
        assert_eq!(
            CHECK_NAMES,
            ["host_key", "ssh", "sudo", "systemd", "release_dir"]
        );
        assert_eq!(
            BUILD_HOST_CHECK_NAMES,
            ["host_key", "ssh", "workspace", "git"]
        );
    }

    #[test]
    fn a_build_host_is_not_checked_for_sudo_or_systemd() {
        // Nothing is installed or supervised on a build host. Checking for
        // systemd here would imply it is a place things run, which is the
        // confusion between the two roles that this feature exists to avoid.
        assert!(!BUILD_HOST_CHECK_NAMES.contains(&"sudo"));
        assert!(!BUILD_HOST_CHECK_NAMES.contains(&"systemd"));
        assert!(!BUILD_HOST_CHECK_NAMES.contains(&"release_dir"));
    }

    #[tokio::test]
    async fn an_unreachable_build_host_reports_ssh_as_the_cause() {
        let (ok, checks, warnings) = check_build_host(
            Err(anyhow::anyhow!("connection refused")),
            "/var/lib/nudo/builds",
            false,
        )
        .await;

        assert!(!ok);
        let names: Vec<&str> = checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, BUILD_HOST_CHECK_NAMES.to_vec());
        assert!(warnings.is_empty());

        let ssh = find_build(&checks, "ssh");
        assert!(ssh.detail.contains("connection refused"));
        for check in checks.iter().filter(|c| c.name != "ssh") {
            assert!(
                check.detail.contains("no SSH connection"),
                "expected an unchecked marker, got: {}",
                check.detail
            );
        }
    }

    #[tokio::test]
    async fn a_changed_build_host_key_is_blamed_on_the_host_key() {
        // A build host is handed repository credentials, so connecting to the
        // wrong machine matters at least as much here as for a deploy target.
        let (ok, checks, _) = check_build_host(
            Err(HostKeyChanged {
                host: "10.0.0.9".to_string(),
                port: 22,
                expected_fingerprint: "SHA256:old".to_string(),
                key: "ssh-ed25519 AAAA".to_string(),
                fingerprint: "SHA256:new".to_string(),
            }
            .into()),
            "/var/lib/nudo/builds",
            false,
        )
        .await;

        assert!(!ok);
        let host_key = find_build(&checks, "host_key");
        assert!(!host_key.ok);
        assert!(
            host_key.detail.contains("SHA256:old"),
            "{}",
            host_key.detail
        );
        assert!(
            host_key.detail.contains("SHA256:new"),
            "{}",
            host_key.detail
        );

        let ssh = find_build(&checks, "ssh");
        assert!(
            ssh.detail.contains("host key was refused"),
            "got: {}",
            ssh.detail
        );
    }

    #[tokio::test]
    async fn a_latency_critical_build_host_warns_without_failing_the_check() {
        // The decision on this issue: permitted, not refused. A warning that
        // failed the check would make a deliberate configuration look broken
        // and would gate CI on it.
        let (_, _, warnings) = check_build_host(
            Err(anyhow::anyhow!("unreachable")),
            "/var/lib/nudo/builds",
            true,
        )
        .await;

        assert_eq!(warnings.len(), 1, "got: {warnings:?}");
        assert!(
            warnings[0].contains("latency-critical"),
            "got: {}",
            warnings[0]
        );

        // And the warning is the only difference: the checks themselves are
        // identical to the same host without the flag.
        let (ok_flagged, checks_flagged, _) =
            check_build_host(Err(anyhow::anyhow!("unreachable")), "/w/x", true).await;
        let (ok_plain, checks_plain, _) =
            check_build_host(Err(anyhow::anyhow!("unreachable")), "/w/x", false).await;
        assert_eq!(ok_flagged, ok_plain);
        assert_eq!(checks_flagged.len(), checks_plain.len());
    }

    /// The build-host check with this name.
    fn find_build<'a>(checks: &'a [BuildHostCheck], name: &str) -> &'a BuildHostCheck {
        checks
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no {name} check in {checks:?}"))
    }

    /// The check with this name, which every path is expected to report.
    fn find<'a>(checks: &'a [Check], name: &str) -> &'a Check {
        checks
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no {name} check in {checks:?}"))
    }
}
