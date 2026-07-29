//! Installing, configuring and reloading the proxy on a target.
//!
//! Everything here runs over SSH against a real host. The ordering is the whole
//! of the safety argument:
//!
//! 1. Write the new config to a *temporary* path, never over the live one.
//! 2. Ask Caddy to validate it. A config that does not parse is rejected here,
//!    while the proxy is still serving the previous one.
//! 3. Move it into place and reload through the admin API.
//!
//! Step 2 is what keeps a typo in a domain from taking a host offline. Step 3
//! is a reload rather than a restart because a restart drops every connection
//! on the box, including those of services nobody was changing — and because
//! Caddy's `/load` restores the previous config by itself if the new one fails,
//! which is a better rollback than anything nudo could implement on top.

use anyhow::{Context, bail};
use nudo_proto::{CheckIngressResponse, Route, Service, Target, check_ingress_response::Check};

use super::{BINARY_PATH, CONFIG_PATH, UNIT_NAME, admin_port_of, render, render_unit, routes_for};
use crate::ssh::{SshSession, quote};

/// Where the config is staged before it is validated.
///
/// Beside the real one so the move into place is a rename within a filesystem,
/// which is atomic — a reader can never see a partially written Caddyfile.
const STAGED_CONFIG_PATH: &str = "/etc/caddy/Caddyfile.nudo-new";

/// What a reload did.
pub struct ReloadOutcome {
    pub ok: bool,
    pub error: String,
    pub routes: Vec<Route>,
    /// Caddy's version, when it could be determined.
    pub version: Option<String>,
}

/// Writes the config for this target and reloads the proxy.
///
/// Returns `Ok` with `ok: false` when the proxy rejected the config: that is a
/// diagnosis, not a transport failure, and the caller records it against the
/// target rather than failing the deploy that triggered it. An `Err` means the
/// host could not be reached or the command could not be run at all.
pub async fn reload(
    session: &SshSession,
    target: &Target,
    services: &[Service],
) -> anyhow::Result<ReloadOutcome> {
    let config = render(target, services);
    let routes = routes_for(services);

    // ---- stage ----
    session
        .write_file(STAGED_CONFIG_PATH, config.as_bytes(), Some("0644"))
        .await
        .context("staging the proxy config")?;

    // ---- validate, before anything live has changed ----
    let validate = session
        .exec(&format!(
            "{} validate --config {} --adapter caddyfile 2>&1",
            quote(BINARY_PATH),
            quote(STAGED_CONFIG_PATH)
        ))
        .await
        .context("validating the proxy config")?;

    if !validate.ok() {
        // Leave the live config untouched and take the staged file away, so a
        // later reload cannot pick up something known to be broken.
        let _ = session
            .exec(&format!("rm -f {}", quote(STAGED_CONFIG_PATH)))
            .await;
        return Ok(ReloadOutcome {
            ok: false,
            error: first_useful_line(&validate.stdout, &validate.stderr),
            routes,
            version: None,
        });
    }

    // ---- move into place ----
    session
        .exec(&format!(
            "mv {} {}",
            quote(STAGED_CONFIG_PATH),
            quote(CONFIG_PATH)
        ))
        .await?
        .require_success("installing the proxy config")?;

    // ---- reload ----
    // Through the admin API rather than `systemctl reload`, so the failure is
    // Caddy's own message rather than a systemd exit code. `--force` because
    // Caddy skips a reload it believes is a no-op, and "the config on disk
    // changed but the proxy did not pick it up" is a confusing state to debug.
    let admin = admin_port_of(&target.ingress.clone().unwrap_or_default());
    let reload = session
        .exec(&format!(
            "{} reload --config {} --adapter caddyfile --address 127.0.0.1:{} --force 2>&1",
            quote(BINARY_PATH),
            quote(CONFIG_PATH),
            admin
        ))
        .await
        .context("reloading the proxy")?;

    if !reload.ok() {
        return Ok(ReloadOutcome {
            ok: false,
            error: first_useful_line(&reload.stdout, &reload.stderr),
            routes,
            version: None,
        });
    }

    Ok(ReloadOutcome {
        ok: true,
        error: String::new(),
        routes,
        version: caddy_version(session).await,
    })
}

/// Installs Caddy and its unit, if it is not already there.
///
/// Idempotent: an existing binary of the right shape is left alone, so enabling
/// ingress on a host that already has Caddy does not replace what is running.
pub async fn install(session: &SshSession) -> anyhow::Result<String> {
    // Already present is the common case on every call after the first.
    if let Some(version) = caddy_version(session).await {
        install_unit(session).await?;
        return Ok(version);
    }

    // The distribution package is preferred over downloading a binary: it comes
    // with the `caddy` user this unit runs as, and it is what an operator would
    // have installed by hand. Falling back to a direct download would mean
    // creating the user too, and picking an architecture — more moving parts in
    // the step most likely to run unattended.
    let install = session
        .exec(
            "set -e; \
             if command -v apt-get >/dev/null 2>&1; then \
               apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq caddy; \
             elif command -v dnf >/dev/null 2>&1; then \
               dnf install -y -q caddy; \
             elif command -v yum >/dev/null 2>&1; then \
               yum install -y -q caddy; \
             elif command -v apk >/dev/null 2>&1; then \
               apk add --no-cache caddy; \
             else \
               echo 'no supported package manager found' >&2; exit 1; \
             fi",
        )
        .await
        .context("installing caddy")?;

    if !install.ok() {
        bail!(
            "could not install Caddy on this host: {}. Install it yourself and \
             re-run, or set ingress to external and manage the proxy directly",
            first_useful_line(&install.stdout, &install.stderr)
        );
    }

    install_unit(session).await?;

    caddy_version(session)
        .await
        .ok_or_else(|| anyhow::anyhow!("Caddy installed but does not report a version"))
}

/// Writes nudo's unit and enables it.
///
/// Overwrites whatever unit the package shipped. That is deliberate: nudo's has
/// the loopback-only admin API and the reload-not-restart behaviour the rest of
/// this depends on, and a package unit that differs would make the proxy behave
/// differently depending on how it was installed.
async fn install_unit(session: &SshSession) -> anyhow::Result<()> {
    let unit_path = format!("/etc/systemd/system/{UNIT_NAME}");
    session
        .write_file(&unit_path, render_unit().as_bytes(), Some("0644"))
        .await
        .context("writing the proxy unit")?;

    // The unit runs as `caddy`; the package creates that user, but a host where
    // the binary arrived another way may not have it.
    session
        .exec(
            "id -u caddy >/dev/null 2>&1 || \
             useradd --system --home /var/lib/caddy --create-home --shell /usr/sbin/nologin caddy",
        )
        .await?
        .require_success("creating the caddy user")?;

    session
        .exec("systemctl daemon-reload")
        .await?
        .require_success("systemctl daemon-reload")?;
    session
        .exec(&format!("systemctl enable {}", quote(UNIT_NAME)))
        .await?
        .require_success("enabling the proxy unit")?;

    Ok(())
}

/// Starts the proxy if it is not running.
pub async fn ensure_running(session: &SshSession) -> anyhow::Result<()> {
    let active = session
        .exec(&format!(
            "systemctl is-active --quiet {} && echo running || echo stopped",
            quote(UNIT_NAME)
        ))
        .await?;

    if active.trimmed() == "running" {
        return Ok(());
    }

    let start = session
        .exec(&format!("systemctl start {}", quote(UNIT_NAME)))
        .await?;
    if !start.ok() {
        let status = session
            .exec(&format!(
                "systemctl status --no-pager --lines=20 {} 2>&1 || true",
                quote(UNIT_NAME)
            ))
            .await
            .map(|r| r.stdout)
            .unwrap_or_default();
        bail!("the proxy would not start:\n{status}");
    }
    Ok(())
}

/// Stops and disables the proxy, leaving its config on disk.
///
/// The config stays because an operator turning ingress off to debug something
/// should not have to reconstruct it, and a file nothing reads is inert.
pub async fn stop(session: &SshSession) -> anyhow::Result<()> {
    let _ = session
        .exec(&format!("systemctl disable --now {}", quote(UNIT_NAME)))
        .await;
    Ok(())
}

/// Diagnoses whether ingress on this target can actually serve its domains.
///
/// The check that matters is DNS. A domain whose record does not point at this
/// host cannot be issued a certificate, and Caddy retries indefinitely without
/// saying so anywhere an operator looks — the request simply hangs or serves
/// the wrong certificate. That is the single most common way this feature
/// disappoints someone, so it is diagnosed explicitly.
///
/// A domain that does not resolve here is a *warning*, not a failure: the
/// operator may be about to create the record, and failing the check would make
/// a normal step of setting this up look broken.
pub async fn check(
    session: &SshSession,
    target: &Target,
    services: &[Service],
) -> CheckIngressResponse {
    let mut checks = Vec::new();
    let mut warnings = Vec::new();

    // ---- installed ----
    let version = caddy_version(session).await;
    checks.push(Check {
        name: "installed".to_string(),
        ok: version.is_some(),
        detail: match &version {
            Some(version) => format!("caddy {version}"),
            None => format!("no caddy binary at {BINARY_PATH}"),
        },
    });

    // ---- running ----
    let active = session
        .exec(&format!(
            "systemctl is-active {} 2>&1 || true",
            quote(UNIT_NAME)
        ))
        .await
        .map(|r| r.trimmed().to_string())
        .unwrap_or_else(|error| format!("{error:#}"));
    checks.push(Check {
        name: "running".to_string(),
        ok: active == "active",
        detail: format!("{UNIT_NAME} is {active}"),
    });

    // ---- admin api ----
    // Reached from the host itself, because it is bound to loopback.
    let admin = admin_port_of(&target.ingress.clone().unwrap_or_default());
    let admin_probe = session
        .exec(&format!(
            "curl -fsS --max-time 5 http://127.0.0.1:{admin}/config/ >/dev/null 2>&1 \
             && echo ok || echo unreachable"
        ))
        .await
        .map(|r| r.trimmed().to_string())
        .unwrap_or_else(|_| "unreachable".to_string());
    checks.push(Check {
        name: "admin_api".to_string(),
        ok: admin_probe == "ok",
        detail: if admin_probe == "ok" {
            format!("answering on 127.0.0.1:{admin}")
        } else {
            format!("nothing answering on 127.0.0.1:{admin}")
        },
    });

    // ---- config matches what nudo would write ----
    let expected = render(target, services);
    let on_disk = session
        .exec(&format!("cat {} 2>/dev/null || true", quote(CONFIG_PATH)))
        .await
        .map(|r| r.stdout)
        .unwrap_or_default();
    let matches = on_disk.trim() == expected.trim();
    checks.push(Check {
        name: "config".to_string(),
        ok: matches,
        detail: if matches {
            "the config on the host is what nudo would write".to_string()
        } else if on_disk.trim().is_empty() {
            format!("{CONFIG_PATH} is missing or empty; a reload will write it")
        } else {
            "the config on the host differs from what nudo would write; \
             a reload will replace it"
                .to_string()
        },
    });

    // ---- DNS, per domain ----
    let addresses = host_addresses(session).await;
    for route in routes_for(services) {
        let resolved = resolve(session, &route.domain).await;

        if resolved.is_empty() {
            warnings.push(format!(
                "{} does not resolve yet. Create an A or AAAA record pointing it \
                 at this host, or a certificate cannot be issued.",
                route.domain
            ));
            checks.push(Check {
                name: route.domain.clone(),
                ok: false,
                detail: "does not resolve".to_string(),
            });
            continue;
        }

        // Only claim a mismatch when the host's own addresses are known. Behind
        // NAT — a very ordinary way to run this — the public address is not one
        // the host can see, and reporting every such domain as misconfigured
        // would make the check useless exactly where it is needed.
        let points_here = addresses.iter().any(|a| resolved.contains(a));
        if points_here || addresses.is_empty() {
            checks.push(Check {
                name: route.domain.clone(),
                ok: true,
                detail: format!("resolves to {}", resolved.join(", ")),
            });
        } else {
            warnings.push(format!(
                "{} resolves to {} but this host answers on {}. If that is a proxy \
                 or a load balancer in front, this is fine; if not, the certificate \
                 cannot be issued.",
                route.domain,
                resolved.join(", "),
                addresses.join(", ")
            ));
            checks.push(Check {
                name: route.domain.clone(),
                ok: true,
                detail: format!("resolves to {} (not this host)", resolved.join(", ")),
            });
        }
    }

    CheckIngressResponse {
        ok: checks.iter().all(|check| check.ok),
        checks,
        warnings,
    }
}

/// Caddy's version, or `None` when it is not installed.
async fn caddy_version(session: &SshSession) -> Option<String> {
    let result = session
        .exec(&format!(
            "{} version 2>/dev/null || true",
            quote(BINARY_PATH)
        ))
        .await
        .ok()?;

    let line = result.trimmed();
    if line.is_empty() {
        // The package installs to /usr/bin, a direct download to /usr/local/bin.
        let fallback = session
            .exec("caddy version 2>/dev/null || true")
            .await
            .ok()?;
        let line = fallback.trimmed();
        if line.is_empty() {
            return None;
        }
        return Some(first_word(line));
    }
    Some(first_word(line))
}

/// The addresses this host answers on, for comparing against DNS.
///
/// Best-effort: an empty result means "could not tell", which the DNS check
/// treats as "do not claim a mismatch" rather than as "no addresses".
async fn host_addresses(session: &SshSession) -> Vec<String> {
    let result = session
        .exec(
            "ip -o addr show scope global 2>/dev/null | awk '{print $4}' | cut -d/ -f1 \
             || hostname -I 2>/dev/null || true",
        )
        .await;

    let Ok(result) = result else {
        return Vec::new();
    };
    result
        .stdout
        .split_whitespace()
        .map(str::to_string)
        .filter(|a| !a.is_empty())
        .collect()
}

/// Resolves a domain from the target's own vantage point.
///
/// From the target rather than from the control plane on purpose: split-horizon
/// DNS is common, and what matters for certificate issuance is what the world —
/// and the host — sees, not what the control plane's resolver happens to hold.
async fn resolve(session: &SshSession, domain: &str) -> Vec<String> {
    // A wildcard cannot be resolved; check the domain it would cover instead, so
    // "*.example.com" reports on "example.com" rather than on nothing.
    let lookup = domain.strip_prefix("*.").unwrap_or(domain);

    let result = session
        .exec(&format!(
            "getent ahostsv4 {host} 2>/dev/null | awk '{{print $1}}' | sort -u \
             || nslookup {host} 2>/dev/null | awk '/^Address: /{{print $2}}' || true",
            host = quote(lookup)
        ))
        .await;

    let Ok(result) = result else {
        return Vec::new();
    };
    let mut out: Vec<String> = result
        .stdout
        .split_whitespace()
        .map(str::to_string)
        .filter(|a| !a.is_empty())
        .collect();
    out.dedup();
    out
}

/// The first line of output worth showing an operator.
///
/// Caddy writes its diagnostics to stderr but `2>&1` in the commands above
/// merges them, and either stream can carry the message. Blank lines and the
/// timestamped log preamble are skipped so the reported error is the error.
fn first_useful_line(stdout: &str, stderr: &str) -> String {
    for stream in [stderr, stdout] {
        if let Some(line) = stream
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty() && !line.starts_with("{\"level\""))
        {
            return line.to_string();
        }
    }
    "the proxy rejected the config without saying why".to_string()
}

fn first_word(line: &str) -> String {
    line.split_whitespace()
        .next()
        .unwrap_or(line)
        .trim_start_matches('v')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reported_error_skips_caddys_json_log_preamble() {
        // Caddy logs structured lines before the message that matters; showing
        // the first line verbatim would report a log entry as the error.
        let stderr = "{\"level\":\"info\",\"msg\":\"using config\"}\n\
                      Error: adapting config: Caddyfile:7: unrecognized directive";
        assert_eq!(
            first_useful_line("", stderr),
            "Error: adapting config: Caddyfile:7: unrecognized directive"
        );
    }

    #[test]
    fn stderr_is_preferred_but_stdout_is_used_when_it_is_empty() {
        assert_eq!(first_useful_line("from stdout", ""), "from stdout");
        assert_eq!(
            first_useful_line("from stdout", "from stderr"),
            "from stderr"
        );
    }

    #[test]
    fn a_silent_failure_still_reports_something() {
        // An empty message rendered in the dashboard as a blank red box would be
        // worse than saying nothing was said.
        assert!(!first_useful_line("", "").is_empty());
        assert!(!first_useful_line("  \n \n", "\n").is_empty());
    }

    #[test]
    fn a_version_line_is_reduced_to_the_version() {
        assert_eq!(first_word("v2.7.6 h1:abcdef"), "2.7.6");
        assert_eq!(first_word("2.7.6"), "2.7.6");
    }
}
